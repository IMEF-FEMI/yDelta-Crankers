//! Prometheus exposition + counter/histogram helpers.
//!
//! Boot path (called once from `main`):
//!   1. `install(bind_addr)` starts the `/metrics` HTTP listener.
//!   2. Each handler emits via `metrics::counter!` / `metrics::histogram!`.
//!   3. Grafana Cloud (or any Prometheus) scrapes `<bind_addr>/metrics`.
//!
//! Metric naming convention:
//!   ydelta_cranker_<scope>_<unit>
//!     e.g. `ydelta_cranker_ticks_total`
//!          `ydelta_cranker_tick_duration_seconds`
//!          `ydelta_cranker_ixs_submitted_total`
//!          `ydelta_cranker_ix_latency_seconds`
//!
//! Labels:
//!   - `handler`  — one of "promoter" / "claimer" / "liquidator" /
//!                  "policy_sync" / "curator_keeper"
//!   - `ix`       — instruction tag name (e.g. "process_matched_loan")
//!   - `outcome`  — "ok" | "err"

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use metrics_exporter_prometheus::PrometheusBuilder;

pub const M_TICKS_TOTAL: &str = "ydelta_cranker_ticks_total";
pub const M_TICK_DURATION: &str = "ydelta_cranker_tick_duration_seconds";
pub const M_CANDIDATES_SEEN: &str = "ydelta_cranker_candidates_seen_total";
pub const M_IXS_SUBMITTED: &str = "ydelta_cranker_ixs_submitted_total";
pub const M_IX_LATENCY: &str = "ydelta_cranker_ix_latency_seconds";
pub const M_SIGNER_SOL_BALANCE: &str = "ydelta_cranker_signer_sol_balance";
/// Counter for failed handler ticks, labelled by reason so dashboards
/// can distinguish RPC outages, sim rejects, insufficient-funds, etc.
/// Incremented from the supervisor on `Err` returns from `tick()`.
pub const M_TX_FAILURES_TOTAL: &str = "ydelta_cranker_tx_failures_total";

/// Stable reason labels for `M_TX_FAILURES_TOTAL`. Limited cardinality
/// (Prometheus dimensionality limits) — keep this list small.
pub const FAIL_REASON_RPC: &str = "rpc_error";
pub const FAIL_REASON_INSUFFICIENT_FUNDS: &str = "insufficient_funds";
pub const FAIL_REASON_SIM_FAILED: &str = "sim_failed";
pub const FAIL_REASON_TX_REJECTED: &str = "tx_rejected";
pub const FAIL_REASON_INDEXER: &str = "indexer_error";
pub const FAIL_REASON_DECODE: &str = "decode_error";
pub const FAIL_REASON_INTERNAL: &str = "internal";

/// Classify a handler tick error into one of the FAIL_REASON_* buckets.
/// Best-effort substring match on the error chain; missed cases fall
/// through to `internal` (which is what we'd want for "weird, look at
/// the log").
pub fn classify_handler_error(err: &anyhow::Error) -> &'static str {
    let msg = format!("{err:#}").to_lowercase();
    if msg.contains("indexer") || msg.contains("reqwest") || msg.contains("http") {
        FAIL_REASON_INDEXER
    } else if msg.contains("insufficient") || msg.contains("lamports") || msg.contains("not enough")
    {
        FAIL_REASON_INSUFFICIENT_FUNDS
    } else if msg.contains("simulation") || msg.contains("simulate") {
        FAIL_REASON_SIM_FAILED
    } else if msg.contains("blockhash")
        || msg.contains("send_and_confirm")
        || msg.contains("tx_rejected")
    {
        FAIL_REASON_TX_REJECTED
    } else if msg.contains("rpc") || msg.contains("connection") || msg.contains("timed out") {
        FAIL_REASON_RPC
    } else if msg.contains("decode") || msg.contains("deserialize") || msg.contains("bytemuck") {
        FAIL_REASON_DECODE
    } else {
        FAIL_REASON_INTERNAL
    }
}

pub fn install(bind: SocketAddr) -> Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(bind)
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(M_TICK_DURATION.to_string()),
            // Tick durations: a few ms to a minute.
            &[0.005, 0.025, 0.1, 0.5, 1.0, 5.0, 30.0, 60.0],
        )
        .ok()
        .ok_or_else(|| anyhow::anyhow!("set_buckets_for_metric: tick_duration"))?
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(M_IX_LATENCY.to_string()),
            // Ix latency: confirmation-bound. Solana txs land in 0.5-30s typically.
            &[0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0],
        )
        .ok()
        .ok_or_else(|| anyhow::anyhow!("set_buckets_for_metric: ix_latency"))?
        .install()
        .context("PrometheusBuilder::install")?;
    Ok(())
}

/// Spawned by `main`: every 60s, refresh signer SOL balance gauges so
/// dashboards can alert before tx fees starve the bots. Reads the
/// fee-payer + every curator.
pub fn spawn_sol_balance_loop(
    rpc: crate::rpc::Rpc,
    signers: crate::signer::Signers,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fee_payer_low: std::sync::Arc<std::sync::atomic::AtomicBool>,
    min_balance_lamports: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use solana_sdk::signature::Signer as _;
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let client = rpc.client();
            let fee_payer_pk = signers.fee_payer.pubkey();
            let mut signers_to_report: Vec<(String, solana_program::pubkey::Pubkey)> =
                vec![("fee_payer".to_string(), fee_payer_pk)];
            for ((vault, profile_id), kp) in signers.curators.iter() {
                signers_to_report.push((format!("curator:{vault}:{profile_id}"), kp.pubkey()));
            }
            for (label, pk) in signers_to_report {
                match client.get_balance(&pk).await {
                    Ok(lamports) => {
                        let sol = lamports as f64 / 1_000_000_000.0;
                        metrics::gauge!(M_SIGNER_SOL_BALANCE, "signer" => label.clone(), "pubkey" => pk.to_string())
                            .set(sol);
                        // Flip the fee-payer-low flag on threshold
                        // crossings. We log on each transition so the
                        // operator can see the pause / resume in the
                        // log stream, not just in metrics.
                        if pk == fee_payer_pk {
                            let was_low = fee_payer_low.load(std::sync::atomic::Ordering::Relaxed);
                            let is_low = lamports < min_balance_lamports;
                            if is_low != was_low {
                                fee_payer_low.store(is_low, std::sync::atomic::Ordering::Relaxed);
                                if is_low {
                                    tracing::warn!(
                                        balance_sol = sol,
                                        threshold_sol = min_balance_lamports as f64 / 1e9,
                                        "fee-payer below threshold; pausing fee-payer handlers",
                                    );
                                } else {
                                    tracing::info!(
                                        balance_sol = sol,
                                        "fee-payer back above threshold; resuming handlers",
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, signer = %label, "get_balance failed");
                    }
                }
            }
            // Tokio sleep that wakes on stop.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                tokio::time::sleep(
                    deadline
                        .saturating_duration_since(now)
                        .min(Duration::from_secs(1)),
                )
                .await;
            }
        }
    })
}
