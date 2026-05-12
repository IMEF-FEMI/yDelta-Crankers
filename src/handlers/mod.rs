//! Handler trait + supervisor.
//!
//! Each handler runs its own loop on its own interval, sharing the RPC,
//! chain reader, and signer references. A shared `stop` atomic
//! propagates sigterm/sigint to every loop. Failures inside a tick log
//! + sleep + continue; we never let one bad tick crash the whole bot.

use std::{
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use async_trait::async_trait;
use tokio::task::JoinHandle;

use crate::{chain_reader::ChainReader, rpc::Rpc, signer::Signers};

pub mod claimer;
pub mod curator_keeper;
pub mod liquidator;
pub mod policy_sync;
pub mod promoter;
pub mod util;

/// Shared, immutable context every handler gets.
#[derive(Clone)]
pub struct HandlerContext {
    pub cfg: Arc<crate::config::Config>,
    pub rpc: Rpc,
    pub chain: ChainReader,
    pub signers: Signers,
    pub stop: Arc<AtomicBool>,
    /// Set when fee-payer balance drops below
    /// `cfg.min_signer_balance_lamports`. The supervisor skips ticks
    /// for handlers where `requires_fee_payer()` is true while this
    /// flag is set. curator_keeper signs with per-curator keypairs and
    /// is unaffected.
    pub fee_payer_low: Arc<AtomicBool>,
}

/// A handler is a periodic tick. Implementors do one unit of work in
/// `tick()`; the supervisor handles intervals, error logging, and
/// graceful shutdown.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    /// Stable name for log filtering.
    fn name(&self) -> &'static str;

    /// Does this handler sign with the fee payer? When true, the
    /// supervisor pauses ticks while `ctx.fee_payer_low` is set, so a
    /// drained fee-payer doesn't spew rejected txs. Default true —
    /// only curator_keeper overrides.
    fn requires_fee_payer(&self) -> bool {
        true
    }

    /// One iteration of work. Returning Err just logs; never crashes
    /// the loop. Implementors should treat "no candidates" as Ok(()).
    async fn tick(&self, ctx: &HandlerContext) -> anyhow::Result<()>;
}

/// Run a handler in its own task. Returns the JoinHandle so `main` can
/// await graceful shutdown.
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
            // Skip the tick if the fee-payer is starved and this
            // handler signs with it. Counted as a distinct outcome so
            // dashboards can detect "bot is alive but paused waiting
            // on top-up" without conflating it with normal idle ticks.
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
            // Sleep up to the interval, waking early on stop.
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
pub use curator_keeper::CuratorKeeperHandler;
pub use liquidator::LiquidatorHandler;
pub use policy_sync::PolicySyncHandler;
pub use promoter::PromoterHandler;
