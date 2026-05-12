//! `SyncMarketSeatsForRiskProfile` (tag 38) — re-stamps cached
//! `risk_profile_max_ltv_bps` on each market-side vault seat after the
//! curator calls `UpdateRiskProfile`.
//!
//! Trigger: indexer event `risk_profile_updated` since our last cursor.
//! For each event, fetch the live `RiskProfile`, batch its
//! `active_markets` into chunks of ≤8, and fire one ix per batch.
//!
//! Signer rules: permissionless. Fee payer signs.

use std::{str::FromStr, sync::Mutex};

use anyhow::Result;
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::sync_market_seats_for_risk_profile_instruction;

use super::{Handler, HandlerContext};

const EVENT_KIND: &str = "risk_profile_updated";
const MARKETS_PER_IX: usize = 8;

pub struct PolicySyncHandler {
    last_seen_slot: Mutex<Option<i64>>,
}

impl PolicySyncHandler {
    pub fn new() -> Self {
        Self {
            last_seen_slot: Mutex::new(None),
        }
    }
}

#[async_trait]
impl Handler for PolicySyncHandler {
    fn name(&self) -> &'static str {
        "policy_sync"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        let from_slot = *self.last_seen_slot.lock().unwrap();
        let events = ctx
            .indexer
            .events(crate::indexer_client::EventsQuery {
                kinds: vec![EVENT_KIND.to_string()],
                from_slot,
                limit: Some(200),
                ..Default::default()
            })
            .await?;
        if events.is_empty() {
            return Ok(());
        }

        let max_slot = events.iter().map(|e| e.slot).max();
        tracing::info!(
            event_count = events.len(),
            max_slot,
            "processing risk_profile_updated events"
        );

        for ev in &events {
            let Some(vault_str) = ev.global_vault.as_ref() else {
                tracing::warn!(slot = ev.slot, "risk_profile_updated missing global_vault");
                continue;
            };
            let Some(profile_id) = ev.profile_id else {
                tracing::warn!(slot = ev.slot, "risk_profile_updated missing profile_id");
                continue;
            };
            let vault = match Pubkey::from_str(vault_str) {
                Ok(pk) => pk,
                Err(e) => {
                    tracing::warn!(error = %e, "bad vault pubkey on event");
                    continue;
                }
            };
            if let Err(e) = self
                .sync_profile_markets(ctx, &vault, profile_id as u8)
                .await
            {
                tracing::warn!(vault = %vault, profile_id, error = %e, "sync_profile_markets failed");
            }
        }

        // Advance cursor (best effort). The re-stamp is idempotent —
        // a re-fired event on retry just no-ops if the seat already
        // matches the live profile.
        if let Some(s) = max_slot {
            *self.last_seen_slot.lock().unwrap() = Some(s + 1);
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
        let profile = ctx.indexer.risk_profile(vault, profile_id).await?;
        let mut markets = Vec::new();
        for m in &profile.active_markets {
            markets.push(Pubkey::from_str(m)?);
        }
        if markets.is_empty() {
            return Ok(());
        }

        // The ix builder needs the debt_mint to derive the vault PDA.
        // Pull it from any of the profile's markets (they all share
        // the same debt mint — the vault is per-mint).
        let market_view = ctx
            .indexer
            .markets()
            .await?
            .into_iter()
            .find(|m| m.address == markets[0].to_string())
            .ok_or_else(|| anyhow::anyhow!("market {} not in indexer", markets[0]))?;
        let debt_mint = Pubkey::from_str(&market_view.debt_mint)?;

        let fee_payer = ctx.signers.fee_payer.clone();
        let fee_payer_pk = fee_payer.pubkey();
        for chunk in markets.chunks(MARKETS_PER_IX) {
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
