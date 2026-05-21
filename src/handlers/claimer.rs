//! `ClaimRepaymentForRiskProfile` (tag 20). Closes the rent loop the
//! promoter opened by passing the fee payer as `cranker_refund`.

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::claim_repayment_for_risk_profile_instruction;
use ydelta::state::OWNER_KIND_RISK_PROFILE;
use ydelta::validation::get_lender_integration_account_address;

use crate::chain_reader::{LoanView, MarketView};

use super::util::now_unix;
use super::{Handler, HandlerContext};

pub struct ClaimerHandler {
    inflight: Mutex<HashSet<Pubkey>>,
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
        let loans = ctx.chain.list_repaid_vault_loans().await?;
        if loans.is_empty() {
            return Ok(());
        }

        let t_now = now_unix();
        let markets = ctx.chain.list_markets().await?;
        let markets_by_pk: HashMap<Pubkey, &MarketView> =
            markets.iter().map(|m| (m.address, m)).collect();

        let candidates: Vec<&LoanView> = loans
            .iter()
            .filter(|l| l.lender_kind == OWNER_KIND_RISK_PROFILE)
            .filter(|l| t_now >= l.matures_at_unix)
            .collect();

        let mut claimed = 0;
        for loan in candidates {
            let loan_pk = loan.address;
            if !self.claim_inflight(loan_pk) {
                continue;
            }
            let res = self.claim_one(ctx, loan, &markets_by_pk).await;
            self.release_inflight(loan_pk);
            match res {
                Ok(()) => claimed += 1,
                Err(e) => tracing::warn!(loan = %loan_pk, error = %e, "claim_one failed"),
            }
        }
        if claimed > 0 {
            tracing::info!(claimed, "claimer tick");
        }
        Ok(())
    }
}

impl ClaimerHandler {
    fn claim_inflight(&self, loan: Pubkey) -> bool {
        self.inflight.lock().unwrap().insert(loan)
    }

    fn release_inflight(&self, loan: Pubkey) {
        self.inflight.lock().unwrap().remove(&loan);
    }

    async fn claim_one(
        &self,
        ctx: &HandlerContext,
        loan: &LoanView,
        markets_by_pk: &HashMap<Pubkey, &MarketView>,
    ) -> Result<()> {
        let market_pk = loan.market;
        let market = *markets_by_pk
            .get(&market_pk)
            .ok_or_else(|| anyhow!("market {market_pk} not found on chain"))?;
        let debt_mint = market.debt_mint;
        let banks = ctx.cfg.banks_snapshot();
        let debt_bank = banks
            .get(&debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for debt mint {debt_mint}"))?
            .clone();

        let (lender_marginfi, _) = get_lender_integration_account_address(&market_pk);

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        let ix = claim_repayment_for_risk_profile_instruction(
            &payer_pk,
            &market_pk,
            loan.matched_loan_sequence,
            &loan.lender_global_vault,
            &debt_mint,
            &debt_bank.bank,
            &debt_bank.liquidity_vault,
            &debt_bank.liquidity_vault_authority,
            // `load_vault_settle_accounts` reads exactly one oracle.
            &debt_bank.primary_oracle(),
            &lender_marginfi,
            &debt_bank.token_program,
            &ctx.cfg.marginfi_group,
            &ctx.cfg.marginfi_program_id,
            Some(&payer_pk),
        );

        let sig = ctx
            .rpc
            .send_signed_labeled("claim_repayment_for_risk_profile", vec![ix], &[&fee_payer])
            .await?;
        tracing::info!(
            loan = %loan.address,
            vault = %loan.lender_global_vault,
            profile_id = loan.lender_profile_id,
            sig = %sig,
            "vault loan claimed"
        );
        Ok(())
    }
}
