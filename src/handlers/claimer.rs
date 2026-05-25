//! `ClaimRepaymentForRiskProfile` (tag 20). Stateless per-(profile, market)
//! sweeper. The on-chain processor moves `debt_withdrawable_shares` from
//! the per-market `lender_marginfi_account` into the per-vault marginfi
//! integration account, then decrements both the seat shares and
//! `profile.pending_claim_atoms`. Repay / liquidate / settle do their own
//! close-out + loan-PDA close in-tx; the cranker just sweeps leftover
//! seat shares.

use std::{collections::HashSet, sync::Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::claim_repayment_for_risk_profile_instruction;
use ydelta::validation::get_lender_integration_account_address;

use crate::chain_reader::PendingVaultClaim;

use super::{Handler, HandlerContext};

pub struct ClaimerHandler {
    /// Deduplicates concurrent sweep attempts within a tick by (market, vault, profile_id).
    inflight: Mutex<HashSet<(Pubkey, Pubkey, u8)>>,
}

impl ClaimerHandler {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for ClaimerHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Handler for ClaimerHandler {
    fn name(&self) -> &'static str {
        "claimer"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        let markets = ctx.chain.list_markets().await?;
        if markets.is_empty() {
            return Ok(());
        }
        let pending = ctx.chain.list_pending_vault_claims(&markets).await?;
        if pending.is_empty() {
            return Ok(());
        }

        let mut claimed = 0;
        for entry in &pending {
            let key = (entry.market, entry.lender_global_vault, entry.risk_profile_id);
            if !self.claim_inflight(key) {
                continue;
            }
            let res = self.sweep_one(ctx, entry).await;
            self.release_inflight(key);
            match res {
                Ok(()) => claimed += 1,
                Err(e) => tracing::warn!(
                    market = %entry.market,
                    vault = %entry.lender_global_vault,
                    profile_id = entry.risk_profile_id,
                    error = %e,
                    "claim sweep failed"
                ),
            }
        }
        if claimed > 0 {
            tracing::info!(claimed, "claimer tick");
        }
        Ok(())
    }
}

impl ClaimerHandler {
    fn claim_inflight(&self, key: (Pubkey, Pubkey, u8)) -> bool {
        self.inflight.lock().unwrap().insert(key)
    }

    fn release_inflight(&self, key: (Pubkey, Pubkey, u8)) {
        self.inflight.lock().unwrap().remove(&key);
    }

    async fn sweep_one(
        &self,
        ctx: &HandlerContext,
        entry: &PendingVaultClaim,
    ) -> Result<()> {
        let banks = ctx.cfg.banks_snapshot();
        let debt_bank = banks
            .get(&entry.debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for debt mint {}", entry.debt_mint))?
            .clone();

        let (lender_marginfi, _) = get_lender_integration_account_address(&entry.market);

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        let ix = claim_repayment_for_risk_profile_instruction(
            &payer_pk,
            &entry.market,
            entry.risk_profile_id,
            &entry.lender_global_vault,
            &entry.debt_mint,
            &debt_bank.bank,
            &debt_bank.liquidity_vault,
            &debt_bank.liquidity_vault_authority,
            // The processor reads bank oracles for the marginfi withdraw
            // health check; pass all configured oracles.
            &debt_bank.oracles,
            &lender_marginfi,
            &debt_bank.token_program,
            &ctx.cfg.marginfi_group,
            &ctx.cfg.marginfi_program_id,
        );

        let sig = ctx
            .rpc
            .send_signed_labeled("claim_repayment_for_risk_profile", vec![ix], &[&fee_payer])
            .await?;
        tracing::info!(
            market = %entry.market,
            vault = %entry.lender_global_vault,
            profile_id = entry.risk_profile_id,
            shares = entry.debt_withdrawable_shares,
            sig = %sig,
            "vault claim swept"
        );
        Ok(())
    }
}
