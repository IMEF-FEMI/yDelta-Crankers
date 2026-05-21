//! `ClaimCuratorFee` (tag 15). Signs with per-curator keypairs from
//! `CURATOR_KEYPAIRS_JSON`; bypasses the fee-payer-low gate.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer as _};
use ydelta::program::instruction_builders::claim_curator_fee_instruction;

use crate::chain_reader::RiskProfileView;

use super::{Handler, HandlerContext};

pub struct CuratorFeeClaimerHandler {
    inflight: Mutex<HashSet<(Pubkey, u8)>>,
}

impl CuratorFeeClaimerHandler {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for CuratorFeeClaimerHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Handler for CuratorFeeClaimerHandler {
    fn name(&self) -> &'static str {
        "curator_fee_claimer"
    }

    fn requires_fee_payer(&self) -> bool {
        false
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        if ctx.signers.curators.is_empty() {
            return Ok(());
        }

        let profiles = ctx.chain.list_risk_profiles().await?;
        let floor = ctx.cfg.thresholds.min_curator_fee_claim_atoms;
        let banks = ctx.cfg.banks_snapshot();

        let mut claimed = 0usize;
        let mut skipped_no_key = 0usize;
        for profile in &profiles {
            if profile.vault_is_paused {
                continue;
            }
            if profile.accumulated_curator_fee_atoms < floor {
                continue;
            }
            let signer = match ctx.signers.curators.get(&profile.curator) {
                Some(kp) => kp.clone(),
                None => {
                    skipped_no_key += 1;
                    continue;
                }
            };
            if !self.claim_inflight(profile.vault, profile.profile_id) {
                continue;
            }
            let res = self.claim_one(ctx, profile, &banks, signer).await;
            self.release_inflight(profile.vault, profile.profile_id);
            match res {
                Ok(true) => claimed += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    vault = %profile.vault,
                    profile_id = profile.profile_id,
                    error = %e,
                    "claim_curator_fee failed"
                ),
            }
        }
        if claimed > 0 || skipped_no_key > 0 {
            tracing::info!(claimed, skipped_no_key, "curator_fee_claimer tick");
        }
        Ok(())
    }
}

impl CuratorFeeClaimerHandler {
    fn claim_inflight(&self, vault: Pubkey, profile_id: u8) -> bool {
        self.inflight.lock().unwrap().insert((vault, profile_id))
    }
    fn release_inflight(&self, vault: Pubkey, profile_id: u8) {
        self.inflight.lock().unwrap().remove(&(vault, profile_id));
    }

    async fn claim_one(
        &self,
        ctx: &HandlerContext,
        profile: &RiskProfileView,
        banks: &crate::bank_registry::BankRegistry,
        signer: Arc<Keypair>,
    ) -> Result<bool> {
        let curator_pk = signer.pubkey();
        let bank = banks.get(&profile.vault_mint).ok_or_else(|| {
            anyhow!(
                "no BANKS config for vault mint {} (vault {})",
                profile.vault_mint,
                profile.vault
            )
        })?;
        if bank.bank != profile.vault_bank {
            return Err(anyhow!(
                "vault {} bank disagrees with registry: vault.lending_pool={}, registry.bank={}",
                profile.vault,
                profile.vault_bank,
                bank.bank
            ));
        }
        let curator_token = bank.ata_for(&curator_pk);

        let ix = claim_curator_fee_instruction(
            &profile.vault_mint,
            &curator_pk,
            &curator_token,
            &bank.bank,
            &bank.liquidity_vault,
            &bank.liquidity_vault_authority,
            &bank.primary_oracle(),
            &bank.token_program,
            &ctx.cfg.marginfi_program_id,
            &ctx.cfg.marginfi_group,
            profile.profile_id,
        );

        let sig = ctx
            .rpc
            .send_signed_labeled("claim_curator_fee", vec![ix], &[&signer])
            .await?;
        tracing::info!(
            vault = %profile.vault,
            profile_id = profile.profile_id,
            curator = %curator_pk,
            atoms = profile.accumulated_curator_fee_atoms,
            sig = %sig,
            "curator fee claimed"
        );
        Ok(true)
    }
}
