//! `SyncMarketSeatsForRiskProfile` (tag 38) — re-stamps cached
//! `risk_profile_max_ltv_bps` on each market-side vault seat after the
//! curator calls `UpdateRiskProfile`.
//!
//! Trigger: every tick, for every managed `(vault, profile_id)` pair
//! in the curator config, we read the live `RiskProfile` directly from
//! chain and re-stamp its `active_markets`. The on-chain ix is
//! idempotent — a seat that already matches the live profile no-ops
//! safely — so polling-driven sync is correct and avoids any
//! event-feed dependency.
//!
//! Signer rules: permissionless. Fee payer signs.

use anyhow::Result;
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::sync_market_seats_for_risk_profile_instruction;

use super::{Handler, HandlerContext};

const MARKETS_PER_IX: usize = 8;

pub struct PolicySyncHandler;

impl PolicySyncHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Handler for PolicySyncHandler {
    fn name(&self) -> &'static str {
        "policy_sync"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        // Build the (vault, profile_id) target list from the curator
        // config — same set the claimer manages.
        let targets: Vec<(Pubkey, u8)> = ctx
            .cfg
            .curator_signers
            .iter()
            .map(|c| (c.global_vault, c.profile_id))
            .collect();
        if targets.is_empty() {
            return Ok(());
        }

        for (vault, profile_id) in &targets {
            if let Err(e) = self.sync_profile_markets(ctx, vault, *profile_id).await {
                tracing::warn!(
                    vault = %vault,
                    profile_id,
                    error = %e,
                    "sync_profile_markets failed"
                );
            }
        }
        Ok(())
    }
}

impl PolicySyncHandler {
    async fn sync_profile_markets(
        &self,
        ctx: &HandlerContext,
        vault: &Pubkey,
        profile_id: u8,
    ) -> Result<()> {
        // Walk the vault account's `risk_profiles` tree to find this
        // profile's live `active_markets` list.
        let Some(profile) = ctx.chain.read_risk_profile(vault, profile_id).await? else {
            tracing::debug!(
                vault = %vault,
                profile_id,
                "risk profile not found on chain; skipping"
            );
            return Ok(());
        };
        if profile.active_markets.is_empty() {
            return Ok(());
        }

        // Look up the debt_mint for the first market — they all share
        // the same debt mint (the vault is per-mint).
        let markets = ctx.chain.list_markets().await?;
        let debt_mint = markets
            .iter()
            .find(|m| m.address == profile.active_markets[0])
            .map(|m| m.debt_mint)
            .ok_or_else(|| {
                anyhow::anyhow!("market {} not found on chain", profile.active_markets[0])
            })?;

        let fee_payer = ctx.signers.fee_payer.clone();
        let fee_payer_pk = fee_payer.pubkey();
        for chunk in profile.active_markets.chunks(MARKETS_PER_IX) {
            let ix = sync_market_seats_for_risk_profile_instruction(
                &fee_payer_pk,
                &debt_mint,
                profile_id,
                chunk,
            );
            let sig = ctx
                .rpc
                .send_signed_labeled(
                    "sync_market_seats_for_risk_profile",
                    vec![ix],
                    &[&fee_payer],
                )
                .await?;
            tracing::info!(
                vault = %vault,
                profile_id,
                market_count = chunk.len(),
                sig = %sig,
                "policy_sync ix sent"
            );
        }
        Ok(())
    }
}
