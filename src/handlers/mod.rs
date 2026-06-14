use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use tokio::task::JoinHandle;

use crate::{chain_reader::ChainReader, rpc::Rpc, signer::Signers, swb_cranker::SwbCranker};

pub mod claimer;
pub mod curator_fee_claimer;
pub mod liquidator;
pub mod match_cranker;
pub mod promoter;
pub mod util;

#[derive(Clone)]
pub struct HandlerContext {
    pub cfg: Arc<crate::config::Config>,
    pub rpc: Rpc,
    pub chain: ChainReader,
    pub signers: Signers,
    pub stop: Arc<AtomicBool>,
    /// Flipped by `metrics::spawn_sol_balance_loop` on threshold
    /// crossings; the supervisor pauses fee-payer handlers while set.
    pub fee_payer_low: Arc<AtomicBool>,
    pub ata_balances: Arc<RwLock<HashMap<Pubkey, u64>>>,
    /// Switchboard pull-feed cranker. `None` when no Switchboard-collateral
    /// market exists (or boot-time load failed); the liquidator skips the
    /// pre-crank in that case.
    pub swb_cranker: Option<Arc<SwbCranker>>,
}

#[async_trait]
pub trait Handler: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Override to `false` for handlers that sign with a different
    /// keypair (e.g. curator-fee-claimer) so the supervisor's
    /// fee-payer-low gate doesn't pause them.
    fn requires_fee_payer(&self) -> bool {
        true
    }

    /// `Err` is logged and counted; the loop continues.
    async fn tick(&self, ctx: &HandlerContext) -> anyhow::Result<()>;
}

pub fn spawn(handler: Arc<dyn Handler>, ctx: HandlerContext, interval: Duration) -> JoinHandle<()> {
    let name = handler.name();
    tokio::spawn(async move {
        tracing::info!(
            handler = name,
            interval_sec = interval.as_secs(),
            "handler started"
        );
        loop {
            if ctx.stop.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::info!(handler = name, "handler stopping");
                return;
            }
            let should_skip_low_balance = handler.requires_fee_payer()
                && ctx.fee_payer_low.load(std::sync::atomic::Ordering::Relaxed);
            let t0 = std::time::Instant::now();
            let outcome = if should_skip_low_balance {
                tracing::warn!(
                    handler = name,
                    "skipping tick: fee-payer balance below MIN_SIGNER_BALANCE_LAMPORTS",
                );
                "paused_low_balance"
            } else {
                match handler.tick(&ctx).await {
                    Ok(()) => "ok",
                    Err(e) => {
                        let reason = crate::metrics::classify_handler_error(&e);
                        tracing::warn!(
                            handler = name,
                            reason,
                            error = %e,
                            "tick failed",
                        );
                        metrics::counter!(
                            crate::metrics::M_TX_FAILURES_TOTAL,
                            "handler" => name,
                            "reason" => reason,
                        )
                        .increment(1);
                        "err"
                    }
                }
            };
            let elapsed = t0.elapsed();
            metrics::counter!(
                crate::metrics::M_TICKS_TOTAL,
                "handler" => name,
                "outcome" => outcome
            )
            .increment(1);
            metrics::histogram!(
                crate::metrics::M_TICK_DURATION,
                "handler" => name
            )
            .record(elapsed.as_secs_f64());
            if outcome == "ok" {
                tracing::debug!(
                    handler = name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "tick ok"
                );
            }
            let deadline = tokio::time::Instant::now() + interval;
            loop {
                if ctx.stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline.saturating_duration_since(now);
                tokio::time::sleep(remaining.min(Duration::from_secs(1))).await;
            }
        }
    })
}

pub use claimer::ClaimerHandler;
pub use curator_fee_claimer::CuratorFeeClaimerHandler;
pub use liquidator::LiquidatorHandler;
pub use match_cranker::MatchCrankerHandler;
pub use promoter::PromoterHandler;
