//! Prometheus exposition + handler-error classifier.

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use metrics_exporter_prometheus::PrometheusBuilder;

pub const M_TICKS_TOTAL: &str = "ydelta_cranker_ticks_total";
pub const M_TICK_DURATION: &str = "ydelta_cranker_tick_duration_seconds";
pub const M_CANDIDATES_SEEN: &str = "ydelta_cranker_candidates_seen_total";
pub const M_IXS_SUBMITTED: &str = "ydelta_cranker_ixs_submitted_total";
pub const M_IX_LATENCY: &str = "ydelta_cranker_ix_latency_seconds";
pub const M_SIGNER_SOL_BALANCE: &str = "ydelta_cranker_signer_sol_balance";
pub const M_FEE_PAYER_ATA_BALANCE: &str = "ydelta_cranker_fee_payer_ata_atoms";
pub const M_BANK_REGISTRY_SIZE: &str = "ydelta_cranker_bank_registry_size";
pub const M_TX_FAILURES_TOTAL: &str = "ydelta_cranker_tx_failures_total";

// Stable, low-cardinality reason labels for M_TX_FAILURES_TOTAL.
pub const FAIL_REASON_RPC: &str = "rpc_error";
pub const FAIL_REASON_INSUFFICIENT_FUNDS: &str = "insufficient_funds";
pub const FAIL_REASON_SIM_FAILED: &str = "sim_failed";
pub const FAIL_REASON_TX_REJECTED: &str = "tx_rejected";
pub const FAIL_REASON_DECODE: &str = "decode_error";
pub const FAIL_REASON_INTERNAL: &str = "internal";

pub fn classify_handler_error(err: &anyhow::Error) -> &'static str {
    let msg = format!("{err:#}").to_lowercase();
    if msg.contains("insufficient") || msg.contains("lamports") || msg.contains("not enough") {
        FAIL_REASON_INSUFFICIENT_FUNDS
    } else if msg.contains("simulation") || msg.contains("simulate") {
        FAIL_REASON_SIM_FAILED
    } else if msg.contains("blockhash")
        || msg.contains("send_and_confirm")
        || msg.contains("tx_rejected")
    {
        FAIL_REASON_TX_REJECTED
    } else if msg.contains("rpc")
        || msg.contains("connection")
        || msg.contains("timed out")
        || msg.contains("reqwest")
        || msg.contains("http")
    {
        // Network/transport-layer failures (the RPC client uses reqwest/http).
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
            &[0.005, 0.025, 0.1, 0.5, 1.0, 5.0, 30.0, 60.0],
        )
        .ok()
        .ok_or_else(|| anyhow::anyhow!("set_buckets_for_metric: tick_duration"))?
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(M_IX_LATENCY.to_string()),
            &[0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0],
        )
        .ok()
        .ok_or_else(|| anyhow::anyhow!("set_buckets_for_metric: ix_latency"))?
        .install()
        .context("PrometheusBuilder::install")?;
    Ok(())
}

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
            match client.get_balance(&fee_payer_pk).await {
                Ok(lamports) => {
                    let sol = lamports as f64 / 1_000_000_000.0;
                    metrics::gauge!(
                        M_SIGNER_SOL_BALANCE,
                        "signer" => "fee_payer",
                        "pubkey" => fee_payer_pk.to_string(),
                    )
                    .set(sol);
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
                Err(e) => {
                    tracing::debug!(error = %e, "get_balance failed");
                }
            }
            sleep_until_or_stop(&stop, Duration::from_secs(60)).await;
        }
    })
}

async fn sleep_until_or_stop(
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    total: Duration,
) {
    let deadline = tokio::time::Instant::now() + total;
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return;
        }
        tokio::time::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_secs(1)),
        )
        .await;
    }
}

pub fn spawn_ata_balance_loop(
    rpc: crate::rpc::Rpc,
    cfg: std::sync::Arc<crate::config::Config>,
    signers: crate::signer::Signers,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ata_balances: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<solana_program::pubkey::Pubkey, u64>>>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use solana_sdk::signature::Signer as _;
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let fee_payer_pk = signers.fee_payer.pubkey();
            let banks = cfg.banks_snapshot();
            let mut entries: Vec<(solana_program::pubkey::Pubkey, solana_program::pubkey::Pubkey)> =
                Vec::new();
            for mint in banks.mints() {
                let bank = banks.get(mint).expect("just iterated");
                entries.push((*mint, bank.ata_for(&fee_payer_pk)));
            }
            if entries.is_empty() {
                sleep_until_or_stop(&stop, interval).await;
                continue;
            }
            let pubkeys: Vec<_> = entries.iter().map(|(_, a)| *a).collect();
            let accts = match rpc.batch_get_multiple_accounts(&pubkeys).await {
                Ok(a) => a,
                Err(e) => {
                    tracing::debug!(error = %e, "ATA gauge: batch_get_multiple_accounts failed");
                    sleep_until_or_stop(&stop, interval).await;
                    continue;
                }
            };
            let mut new_balances: std::collections::HashMap<solana_program::pubkey::Pubkey, u64> =
                std::collections::HashMap::new();
            for (i, (mint, ata)) in entries.iter().enumerate() {
                let balance = accts
                    .get(i)
                    .and_then(|o| o.as_ref())
                    .and_then(|a| crate::handlers::util::spl_token_amount(&a.data))
                    .unwrap_or(0);
                new_balances.insert(*mint, balance);
                metrics::gauge!(
                    M_FEE_PAYER_ATA_BALANCE,
                    "mint" => mint.to_string(),
                    "ata" => ata.to_string(),
                )
                .set(balance as f64);
            }
            if let Ok(mut guard) = ata_balances.write() {
                *guard = new_balances;
            }
            metrics::gauge!(M_BANK_REGISTRY_SIZE).set(banks.len() as f64);
            sleep_until_or_stop(&stop, interval).await;
        }
    })
}

pub fn spawn_bank_refresh_loop(
    rpc: crate::rpc::Rpc,
    chain: crate::chain_reader::ChainReader,
    cfg: std::sync::Arc<crate::config::Config>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            if let Err(e) = cfg.refresh_banks(&rpc, &chain).await {
                tracing::debug!(error = %e, "bank registry refresh failed");
            }
            sleep_until_or_stop(&stop, interval).await;
        }
    })
}
