//! `ClaimRepaymentForRiskProfile` (tag 24) — drains repaid vault loans
//! back to the GlobalVault.
//!
//! Discovery: for each (vault, profile) we manage (config-driven), pull
//! loans straight from chain via `ChainReader::list_loans_for_profile`
//! (a single `getProgramAccounts` per profile, filtered by
//! `LoanFixed.lender_global_vault` + `lender_profile_id`), filter to
//! `state == Repaid && now >= matures_at_unix`, fire the claim ix.
//!
//! Pass our fee-payer as `cranker_refund` — the on-chain processor
//! refunds the loan PDA's rent to whoever matches `loan.created_by`.
//! Same wallet that runs the promoter handler runs the claimer →
//! rent-neutral pipeline.

use std::{collections::HashSet, sync::Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::claim_repayment_for_risk_profile_instruction;
use ydelta::state::loan::LoanState;
use ydelta::state::OWNER_KIND_RISK_PROFILE;
use ydelta::validation::get_lender_integration_account_address;

use crate::chain_reader::LoanView;

use super::util::now_unix;
use super::{Handler, HandlerContext};

pub struct ClaimerHandler {
    /// (vault, profile_id) targets discovered at first tick from the
    /// curator config. Cached after first build.
    discovered_targets: Mutex<Option<Vec<(Pubkey, u8)>>>,
    inflight: Mutex<HashSet<Pubkey>>,
}

impl ClaimerHandler {
    pub fn new() -> Self {
        Self {
            discovered_targets: Mutex::new(None),
            inflight: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait]
impl Handler for ClaimerHandler {
    fn name(&self) -> &'static str {
        "claimer"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        let targets = self.targets(ctx);
        if targets.is_empty() {
            return Ok(());
        }
        let t_now = now_unix();

        let mut total_claimed = 0;
        for (vault, profile_id) in &targets {
            match self.claim_for_profile(ctx, vault, *profile_id, t_now).await {
                Ok(n) => total_claimed += n,
                Err(e) => tracing::warn!(
                    vault = %vault,
                    profile_id,
                    error = %e,
                    "claim_for_profile failed"
                ),
            }
        }
        if total_claimed > 0 {
            tracing::info!(total_claimed, "claimer tick");
        }
        Ok(())
    }
}

impl ClaimerHandler {
    fn targets(&self, ctx: &HandlerContext) -> Vec<(Pubkey, u8)> {
        if let Some(t) = self.discovered_targets.lock().unwrap().clone() {
            return t;
        }
        // Targets are every (vault, profile_id) the curator config mentions.
        // The fee-payer can claim for any vault loan regardless of who
        // operates the curator — tag 24 is permissionless. We restrict
        // to managed targets to keep the indexer query volume bounded.
        let out: Vec<_> = ctx
            .cfg
            .curator_signers
            .iter()
            .map(|c| (c.global_vault, c.profile_id))
            .collect();
        *self.discovered_targets.lock().unwrap() = Some(out.clone());
        out
    }

    async fn claim_for_profile(
        &self,
        ctx: &HandlerContext,
        vault: &Pubkey,
        profile_id: u8,
        now_unix: i64,
    ) -> Result<usize> {
        let loans = ctx
            .chain
            .list_loans_for_profile(vault, profile_id)
            .await?;

        let candidates: Vec<&LoanView> = loans
            .iter()
            .filter(|l| l.state == LoanState::Repaid as u8)
            .filter(|l| l.lender_kind == OWNER_KIND_RISK_PROFILE)
            .filter(|l| now_unix >= l.matures_at_unix)
            .collect();

        if candidates.is_empty() {
            return Ok(0);
        }

        let mut claimed = 0;
        for c in candidates {
            let loan_pk = c.address;
            if !self.claim_inflight(loan_pk) {
                continue;
            }
            let res = self.claim_one(ctx, vault, c).await;
            self.release_inflight(loan_pk);
            match res {
                Ok(()) => claimed += 1,
                Err(e) => tracing::warn!(loan = %loan_pk, error = %e, "claim_one failed"),
            }
        }
        Ok(claimed)
    }

    fn claim_inflight(&self, loan: Pubkey) -> bool {
        let mut s = self.inflight.lock().unwrap();
        s.insert(loan)
    }

    fn release_inflight(&self, loan: Pubkey) {
        let mut s = self.inflight.lock().unwrap();
        s.remove(&loan);
    }

    async fn claim_one(
        &self,
        ctx: &HandlerContext,
        vault: &Pubkey,
        loan: &LoanView,
    ) -> Result<()> {
        // `matched_loan_sequence` already lives on the `LoanFixed`
        // header, so the chain-side `LoanView` carries it natively —
        // no extra read needed.
        let sequence = loan.matched_loan_sequence;

        let market_pk = loan.market;
        let market_view = ctx
            .chain
            .list_markets()
            .await?
            .into_iter()
            .find(|m| m.address == market_pk)
            .ok_or_else(|| anyhow!("market {market_pk} not found on chain"))?;
        let debt_mint = market_view.debt_mint;
        let debt_bank = ctx
            .cfg
            .banks
            .get(&debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for debt mint {debt_mint}"))?;

        let (lender_marginfi, _) = get_lender_integration_account_address(&market_pk);

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        let ix = claim_repayment_for_risk_profile_instruction(
            &payer_pk,
            &market_pk,
            sequence,
            vault,
            &debt_mint,
            &debt_bank.bank,
            &debt_bank.liquidity_vault,
            &debt_bank.liquidity_vault_authority,
            &debt_bank.primary_oracle(),
            &lender_marginfi,
            &debt_bank.token_program,
            &ctx.cfg.marginfi_group,
            &ctx.cfg.marginfi_program_id,
            Some(&payer_pk), // cranker_refund — recovers tag-7 rent if we created it
        );

        let sig = ctx
            .rpc
            .send_signed_labeled("claim_repayment_for_risk_profile", vec![ix], &[&fee_payer])
            .await?;
        tracing::info!(
            loan = %loan.address,
            sig = %sig,
            "vault loan claimed"
        );
        Ok(())
    }
}
