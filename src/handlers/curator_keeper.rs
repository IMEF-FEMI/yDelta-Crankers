//! `UpdateOrderForRiskProfile` (tag 18) — keeps each managed
//! (vault, profile, market) ask rate at its configured target.
//!
//! Two modes per `CuratorSignerConfig.rate_target`:
//!
//!   - **Static**:  always quote `rate_bps`.
//!   - **Dynamic**: read the market's debt-mint marginfi bank, compute
//!     `target = supply_apr + α × (borrow_apr - supply_apr)`. Fall back
//!     to the configured `fallback_bps` if the bank read fails.
//!
//! In both modes the keeper compares against the live resting order's
//! `rate_bps` (from the indexer) and fires `UpdateOrderForRiskProfile`
//! when the delta exceeds `CURATOR_MIN_DELTA_BPS` and the throttle
//! window has elapsed.

use std::{collections::HashMap, str::FromStr, sync::Mutex, time::Instant};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use ydelta::program::instruction_builders::update_order_for_risk_profile_instruction;

use crate::config::{CuratorSignerConfig, RateTarget};
use crate::marginfi_bank::BankView;
use crate::marginfi_rate::{compute_rates, target_rate_bps};

use super::{Handler, HandlerContext};

const SIDE_ASK: i16 = 1;

pub struct CuratorKeeperHandler {
    /// `(global_vault, profile_id, market) → last update Instant`.
    last_update: Mutex<HashMap<(Pubkey, u8, Pubkey), Instant>>,
}

impl CuratorKeeperHandler {
    pub fn new() -> Self {
        Self {
            last_update: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Handler for CuratorKeeperHandler {
    fn name(&self) -> &'static str {
        "curator_keeper"
    }

    /// Curator keeper signs `UpdateOrderForRiskProfile` with the
    /// per-curator keypair — not the fee payer — so the fee-payer
    /// low-balance flag doesn't gate it. Each curator's own SOL is
    /// tracked separately via `M_SIGNER_SOL_BALANCE`.
    fn requires_fee_payer(&self) -> bool {
        false
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        for cfg in &ctx.cfg.curator_signers {
            for market in &cfg.managed_markets {
                if let Err(e) = self.maintain_one(ctx, cfg, market).await {
                    tracing::warn!(
                        vault = %cfg.global_vault,
                        profile_id = cfg.profile_id,
                        market = %market,
                        error = %e,
                        "maintain_one failed"
                    );
                }
            }
        }
        Ok(())
    }
}

impl CuratorKeeperHandler {
    async fn maintain_one(
        &self,
        ctx: &HandlerContext,
        cfg: &CuratorSignerConfig,
        market: &Pubkey,
    ) -> Result<()> {
        // Throttle by per-(profile, market) cooldown.
        let key = (cfg.global_vault, cfg.profile_id, *market);
        if let Some(prev) = self.last_update.lock().unwrap().get(&key) {
            if prev.elapsed() < ctx.cfg.thresholds.curator_min_update_interval {
                return Ok(());
            }
        }

        // Look up the live order and the market metadata together.
        let orders = ctx.indexer.market_orders(market, None).await?;
        let live = orders.iter().find(|o| {
            o.owner_kind.unwrap_or(-1) == ydelta::state::OWNER_KIND_RISK_PROFILE as i16
                && o.risk_profile_id == Some(cfg.profile_id as i16)
                && o.side == SIDE_ASK
                && o.owner.as_deref() == Some(&cfg.global_vault.to_string())
        });
        let current_bps = match live {
            Some(o) => o.rate_bps as i32,
            None => {
                tracing::info!(
                    vault = %cfg.global_vault,
                    profile_id = cfg.profile_id,
                    market = %market,
                    "no live order for managed (profile, market); use PlaceOrderForRiskProfile manually first"
                );
                return Ok(());
            }
        };

        let market_view = ctx
            .indexer
            .markets()
            .await?
            .into_iter()
            .find(|m| m.address == market.to_string())
            .ok_or_else(|| anyhow!("market {} not in indexer", market))?;
        let debt_mint = Pubkey::from_str(&market_view.debt_mint)?;

        // Resolve the target rate.
        let target_bps = match cfg.rate_target {
            RateTarget::Static { rate_bps } => rate_bps,
            RateTarget::Dynamic {
                alpha_bps,
                fallback_bps,
            } => match self.dynamic_target(ctx, &debt_mint, alpha_bps).await {
                Ok(t) => {
                    tracing::debug!(
                        vault = %cfg.global_vault,
                        profile_id = cfg.profile_id,
                        market = %market,
                        alpha_bps,
                        dynamic_target_bps = t,
                        "dynamic rate computed"
                    );
                    t
                }
                Err(e) => {
                    tracing::warn!(
                        vault = %cfg.global_vault,
                        profile_id = cfg.profile_id,
                        market = %market,
                        error = %e,
                        "dynamic rate computation failed; falling back to {fallback_bps} bps"
                    );
                    fallback_bps
                }
            },
        };

        let delta = (target_bps as i32 - current_bps).unsigned_abs();
        if delta < ctx.cfg.thresholds.curator_min_delta_bps as u32 {
            tracing::debug!(
                vault = %cfg.global_vault,
                profile_id = cfg.profile_id,
                market = %market,
                target_bps,
                current_bps,
                delta,
                "within min delta — skipping"
            );
            return Ok(());
        }

        // Fire the update.
        let curator = ctx.signers.curator_for(cfg)?;
        use solana_sdk::signature::Signer as _;
        let curator_pk = curator.pubkey();
        let ix = update_order_for_risk_profile_instruction(
            &debt_mint,
            market,
            &curator_pk,
            cfg.profile_id,
            target_bps,
            cfg.target_term_seconds,
            0, // flags
        );
        let sig = ctx
            .rpc
            .send_signed_labeled("update_order_for_risk_profile", vec![ix], &[&curator])
            .await?;
        tracing::info!(
            vault = %cfg.global_vault,
            profile_id = cfg.profile_id,
            market = %market,
            old_rate_bps = current_bps,
            new_rate_bps = target_bps,
            sig = %sig,
            "curator order updated"
        );
        self.last_update.lock().unwrap().insert(key, Instant::now());
        Ok(())
    }

    async fn dynamic_target(
        &self,
        ctx: &HandlerContext,
        debt_mint: &Pubkey,
        alpha_bps: u16,
    ) -> Result<u16> {
        let bank = ctx
            .cfg
            .banks
            .get(debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for {debt_mint}"))?;
        let raw = ctx
            .rpc
            .get_account_data(&bank.bank)
            .await?
            .ok_or_else(|| anyhow!("bank {} not found on-chain", bank.bank))?;
        let view = BankView::try_from_account_data(&raw)?;
        let snapshot = compute_rates(&view);
        tracing::trace!(
            mint = %debt_mint,
            util_bps = snapshot.utilization_bps(),
            borrow_bps = snapshot.borrow_apr_bps(),
            supply_bps = snapshot.supply_apr_bps(),
            "marginfi rate snapshot"
        );
        Ok(target_rate_bps(&snapshot, alpha_bps))
    }
}
