//! `SettleMaturedLoan` (tag 16) + `LiquidateLoan` (tag 17), pre-flighted
//! by tag 34 / 35 sims.

use std::{collections::HashSet, sync::Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::{address_lookup_table::AddressLookupTableAccount, instruction::Instruction, signature::Signer as _};
use ydelta::program::instruction_builders::{
    check_ltv_liquidatable_instruction, check_maturity_liquidatable_instruction,
    liquidate_loan_instruction, settle_matured_loan_instruction,
};
use ydelta::state::loan::LoanState;

use crate::chain_reader::{LoanView, MarketView};

use super::util::{min_partial_repay_atoms, now_unix, p2pool_full_repay_staged_atoms};
use super::{Handler, HandlerContext};

const LOAN_STATE_ACTIVE: u8 = LoanState::Active as u8;

/// Program sentinel: `repay_atoms_max == 0` clamps to live outstanding.
const REPAY_FULL: u64 = 0;

/// 1.05× principal — below this a Fixed loan can't plausibly be
/// LTV-breached, so we skip the sim. P2Pool loans bypass this gate
/// entirely (the body field doesn't track live marginfi liability).
const STATIC_LTV_PREFILTER_BPS: u32 = 10_500;

pub struct LiquidatorHandler {
    inflight: Mutex<HashSet<Pubkey>>,
}

impl LiquidatorHandler {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for LiquidatorHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Handler for LiquidatorHandler {
    fn name(&self) -> &'static str {
        "liquidator"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        let markets = ctx.chain.list_markets().await?;
        let mut total = 0usize;
        for market in &markets {
            if market.is_paused {
                continue;
            }
            match self.scan_market(ctx, market).await {
                Ok(n) => total += n,
                Err(e) => {
                    tracing::warn!(market = %market.address, error = %e, "scan_market failed")
                }
            }
        }
        if total > 0 {
            tracing::info!(settled = total, "liquidator tick");
        }
        Ok(())
    }
}

impl LiquidatorHandler {
    async fn scan_market(&self, ctx: &HandlerContext, market: &MarketView) -> Result<usize> {
        let market_pk = market.address;
        let loans = ctx.chain.list_loans_for_market(&market_pk).await?;

        let t_now = now_unix();
        let extra_buffer = ctx.cfg.thresholds.maturity_extra_buffer.as_secs() as i64;
        let effective_grace = i64::from(market.grace_period_seconds) + extra_buffer;

        let mut count = 0;
        for loan in &loans {
            if loan.state != LOAN_STATE_ACTIVE || loan.outstanding_debt_atoms == 0 {
                continue;
            }

            let matured = t_now >= loan.matures_at_unix.saturating_add(effective_grace);
            let maybe_ltv_breach = if loan.is_p2pool() {
                true
            } else {
                static_accrued_ratio_bps(loan.outstanding_debt_atoms, loan.principal_debt_atoms)
                    >= STATIC_LTV_PREFILTER_BPS
            };

            if !matured && !maybe_ltv_breach {
                continue;
            }

            let loan_pk = loan.address;
            if !self.claim_inflight(loan_pk) {
                continue;
            }
            let res = self
                .try_settle_or_liquidate(ctx, market, loan, matured, maybe_ltv_breach)
                .await;
            self.release_inflight(loan_pk);
            match res {
                Ok(true) => count += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(loan = %loan.address, error = %e, "settle/liquidate failed")
                }
            }
        }
        Ok(count)
    }

    fn claim_inflight(&self, loan: Pubkey) -> bool {
        self.inflight.lock().unwrap().insert(loan)
    }
    fn release_inflight(&self, loan: Pubkey) {
        self.inflight.lock().unwrap().remove(&loan);
    }

    /// Build (don't submit) the Switchboard fetch-update bundle for the
    /// collateral feed, so callers can BUNDLE it into both the sim AND
    /// the real submission. Sim runs both in the RPC sandbox — the SWB
    /// update lands in-memory before the consuming check reads the price,
    /// so the sim sees a fresh oracle but pays zero SOL. We only spend
    /// SOL when the consuming check passes and we submit the real tx.
    ///
    /// Returns `(vec![], vec![])` for Pyth-Push collateral (Pyth-DA keeps
    /// that fresh) or when no SWB cranker is configured, so callers can
    /// treat the result as a uniform "prepend these to your ixs/luts".
    /// A gateway failure is non-fatal: the caller proceeds with an empty
    /// bundle and the on-chain staleness gate decides whether to reject.
    async fn fetch_swb_bundle(
        &self,
        ctx: &HandlerContext,
        collateral_bank: &crate::bank_registry::BankInfo,
        loan: &LoanView,
    ) -> (Vec<Instruction>, Vec<AddressLookupTableAccount>) {
        if !collateral_bank.is_switchboard_pull() {
            return (vec![], vec![]);
        }
        let Some(cranker) = ctx.swb_cranker.as_ref() else {
            return (vec![], vec![]);
        };
        match cranker
            .fetch_update_ixs(vec![collateral_bank.primary_oracle()])
            .await
        {
            Ok(bundle) => bundle,
            Err(e) => {
                tracing::warn!(
                    loan = %loan.address,
                    error = %e,
                    "switchboard gateway fetch failed; proceeding without prepend (sim will gate)"
                );
                (vec![], vec![])
            }
        }
    }

    /// Prefer `liquidate_loan` when under-water: it pays the keeper
    /// bonus, `settle_matured_loan` doesn't. Fall back to settle when
    /// the LTV sim refuses but the loan is matured.
    async fn try_settle_or_liquidate(
        &self,
        ctx: &HandlerContext,
        market: &MarketView,
        loan: &LoanView,
        matured: bool,
        maybe_ltv_breach: bool,
    ) -> Result<bool> {
        if maybe_ltv_breach {
            if self.try_liquidate_ltv(ctx, market, loan).await? {
                return Ok(true);
            }
            if matured {
                return self.try_settle_matured(ctx, market, loan).await;
            }
            return Ok(false);
        }
        if matured {
            return self.try_settle_matured(ctx, market, loan).await;
        }
        Ok(false)
    }

    async fn try_settle_matured(
        &self,
        ctx: &HandlerContext,
        market: &MarketView,
        loan: &LoanView,
    ) -> Result<bool> {
        let market_pk = market.address;
        let debt_mint = market.debt_mint;
        let collateral_mint = market.collateral_mint;

        let banks = ctx.cfg.banks_snapshot();
        let (debt_bank, collateral_bank) = bank_pair(&banks, &debt_mint, &collateral_mint)?;
        let debt_bank = debt_bank.clone();
        let collateral_bank = collateral_bank.clone();

        let sequence = loan.matched_loan_sequence;
        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        // The settle ix reads the collateral (Switchboard) feed, so bundle
        // a fresh SWB update in front of BOTH the sim and the real submit.
        // The sim runs the update in-memory (free), so a healthy loan that
        // fails the sim costs us zero SOL — the old flow paid for the SWB
        // update unconditionally before this point.
        let (swb_ixs, swb_luts) = self.fetch_swb_bundle(ctx, &collateral_bank, loan).await;

        let sim_ix = check_maturity_liquidatable_instruction(
            &market_pk,
            &payer_pk,
            sequence,
            &debt_bank.bank,
            &ctx.cfg.marginfi_program_id,
        );
        let mut sim_bundle = swb_ixs.clone();
        sim_bundle.push(sim_ix);
        let sim = ctx.rpc.simulate_v0(sim_bundle, &swb_luts, &payer_pk).await?;
        if !sim.ok {
            tracing::debug!(loan = %loan.address, error = ?sim.error, "maturity sim failed");
            return Ok(false);
        }

        let liquidator_debt_ata = debt_bank.ata_for(&payer_pk);
        let liquidator_collateral_ata = collateral_bank.ata_for(&payer_pk);
        let repay_atoms_max = match self
            .pick_repay_amount(ctx, &liquidator_debt_ata, loan)
            .await?
        {
            Some(v) => v,
            None => return Ok(false),
        };

        // Fixed loans need the lender vault passed through so the full-settle
        // close-out can decrement the risk profile + bump pending_claim_atoms.
        // P2Pool loans have no vault lender — must be None or the loader rejects.
        let settle_global_vault: Option<&Pubkey> = if loan.is_p2pool() {
            None
        } else {
            Some(&loan.lender_global_vault)
        };

        let ix = settle_matured_loan_instruction(
            &market_pk,
            &payer_pk,
            sequence,
            &debt_mint,
            &collateral_mint,
            &liquidator_debt_ata,
            &liquidator_collateral_ata,
            &debt_bank.bank,
            &collateral_bank.bank,
            &debt_bank.liquidity_vault,
            &collateral_bank.liquidity_vault,
            &collateral_bank.liquidity_vault_authority,
            &debt_bank.oracles,
            &collateral_bank.oracles,
            &debt_bank.token_program,
            &ctx.cfg.marginfi_group,
            &ctx.cfg.marginfi_program_id,
            repay_atoms_max,
            &payer_pk,
            settle_global_vault,
        );

        // Same SWB prepend as the sim — the real on-chain landing needs
        // the update too, or the staleness gate inside the settle ix will
        // reject. This is the ONLY place SOL gets spent for the SWB
        // update, and it only fires when sim already said yes.
        let mut real_bundle = swb_ixs;
        real_bundle.push(ix);
        let sig = ctx
            .rpc
            .send_signed_v0_labeled("settle_matured_loan", real_bundle, &swb_luts, &[&fee_payer])
            .await?;
        tracing::info!(
            loan = %loan.address,
            loan_type = loan.loan_type,
            sig = %sig,
            repay_atoms_max,
            "matured loan settled"
        );
        Ok(true)
    }

    async fn try_liquidate_ltv(
        &self,
        ctx: &HandlerContext,
        market: &MarketView,
        loan: &LoanView,
    ) -> Result<bool> {
        let market_pk = market.address;
        let debt_mint = market.debt_mint;
        let collateral_mint = market.collateral_mint;

        let banks = ctx.cfg.banks_snapshot();
        let (debt_bank, collateral_bank) = bank_pair(&banks, &debt_mint, &collateral_mint)?;
        let debt_bank = debt_bank.clone();
        let collateral_bank = collateral_bank.clone();

        let sequence = loan.matched_loan_sequence;
        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        // Bundle a fresh SWB update in front of the LTV sim AND the real
        // liquidate ix. The sim runs both in-memory for free, so a loan
        // whose LTV sim fails costs zero SOL. We only spend on the SWB
        // update when sim says yes and we actually submit.
        let (swb_ixs, swb_luts) = self.fetch_swb_bundle(ctx, &collateral_bank, loan).await;

        let sim_ix = check_ltv_liquidatable_instruction(
            &market_pk,
            &payer_pk,
            sequence,
            &debt_bank.bank,
            &collateral_bank.bank,
            &debt_bank.oracles,
            &collateral_bank.oracles,
            &ctx.cfg.marginfi_program_id,
        );
        let mut sim_bundle = swb_ixs.clone();
        sim_bundle.push(sim_ix);
        let sim = ctx.rpc.simulate_v0(sim_bundle, &swb_luts, &payer_pk).await?;
        if !sim.ok {
            tracing::debug!(loan = %loan.address, error = ?sim.error, "ltv sim failed");
            return Ok(false);
        }

        let expected_bonus = if market.liquidation_keeper_bps > 0 {
            (loan.outstanding_debt_atoms as u128)
                .saturating_mul(market.liquidation_keeper_bps as u128)
                / 10_000u128
        } else {
            loan.outstanding_debt_atoms as u128
        };
        if expected_bonus < ctx.cfg.thresholds.min_liquidation_profit_atoms as u128 {
            tracing::debug!(
                loan = %loan.address,
                expected_bonus = expected_bonus as u64,
                "below min liquidation profit"
            );
            return Ok(false);
        }

        let liquidator_debt_ata = debt_bank.ata_for(&payer_pk);
        let liquidator_collateral_ata = collateral_bank.ata_for(&payer_pk);
        let repay_atoms_max = match self
            .pick_repay_amount(ctx, &liquidator_debt_ata, loan)
            .await?
        {
            Some(v) => v,
            None => return Ok(false),
        };

        // Fixed loans need the lender vault passed through so the full-liquidate
        // close-out can decrement the risk profile + bump pending_claim_atoms.
        // P2Pool loans have no vault lender — must be None or the loader rejects.
        let liquidate_global_vault: Option<&Pubkey> = if loan.is_p2pool() {
            None
        } else {
            Some(&loan.lender_global_vault)
        };

        let ix = liquidate_loan_instruction(
            &market_pk,
            &payer_pk,
            sequence,
            &debt_mint,
            &collateral_mint,
            &liquidator_debt_ata,
            &liquidator_collateral_ata,
            &debt_bank.bank,
            &collateral_bank.bank,
            &debt_bank.liquidity_vault,
            &collateral_bank.liquidity_vault,
            &collateral_bank.liquidity_vault_authority,
            &debt_bank.oracles,
            &collateral_bank.oracles,
            &debt_bank.token_program,
            &ctx.cfg.marginfi_group,
            &ctx.cfg.marginfi_program_id,
            repay_atoms_max,
            &payer_pk,
            liquidate_global_vault,
        );

        // Same SWB prepend as the sim — pays SOL only here, when the
        // sim already confirmed the loan is liquidatable.
        let mut real_bundle = swb_ixs;
        real_bundle.push(ix);
        let sig = ctx
            .rpc
            .send_signed_v0_labeled("liquidate_loan", real_bundle, &swb_luts, &[&fee_payer])
            .await?;
        tracing::info!(
            loan = %loan.address,
            loan_type = loan.loan_type,
            sig = %sig,
            repay_atoms_max,
            "ltv-breach loan liquidated"
        );
        Ok(true)
    }

    /// Pick the largest `repay_atoms_max` the keeper's ATA can fund.
    /// Returns `None` when even the program's 1%/1000-atom partial floor
    /// would overshoot the balance.
    async fn pick_repay_amount(
        &self,
        ctx: &HandlerContext,
        liquidator_debt_ata: &Pubkey,
        loan: &LoanView,
    ) -> Result<Option<u64>> {
        let acct = match ctx.rpc.get_account_data(liquidator_debt_ata).await? {
            Some(d) => d,
            None => {
                tracing::debug!(ata = %liquidator_debt_ata, "liquidator debt ATA missing");
                return Ok(None);
            }
        };
        let ata_balance =
            super::util::spl_token_amount(&acct).ok_or_else(|| anyhow!("ATA data too small"))?;

        // P2Pool body outstanding is stale (no on-chain accrue path);
        // use principal as a more honest floor.
        let outstanding_est: u64 = if loan.is_p2pool() {
            loan.outstanding_debt_atoms.max(loan.principal_debt_atoms)
        } else {
            loan.outstanding_debt_atoms
        };

        let full_required = if loan.is_p2pool() {
            p2pool_full_repay_staged_atoms(outstanding_est)
        } else {
            outstanding_est
        };

        if ata_balance >= full_required {
            return Ok(Some(REPAY_FULL));
        }

        let floor = min_partial_repay_atoms(outstanding_est);
        if ata_balance < floor {
            tracing::debug!(
                loan = %loan.address,
                ata = %liquidator_debt_ata,
                ata_balance,
                floor,
                "ATA below partial-repay floor; skipping"
            );
            return Ok(None);
        }
        // Cap below outstanding so the program routes through the
        // partial path (full-close needs P2Pool over-stage headroom).
        let cap = outstanding_est.saturating_sub(1);
        Ok(Some(ata_balance.min(cap)))
    }
}

fn bank_pair<'a>(
    banks: &'a crate::bank_registry::BankRegistry,
    debt_mint: &Pubkey,
    collateral_mint: &Pubkey,
) -> Result<(
    &'a crate::bank_registry::BankInfo,
    &'a crate::bank_registry::BankInfo,
)> {
    let debt_bank = banks
        .get(debt_mint)
        .ok_or_else(|| anyhow!("no BANKS config for debt mint {debt_mint}"))?;
    let collateral_bank = banks
        .get(collateral_mint)
        .ok_or_else(|| anyhow!("no BANKS config for collateral mint {collateral_mint}"))?;
    Ok((debt_bank, collateral_bank))
}

fn static_accrued_ratio_bps(outstanding_debt_atoms: u64, principal_debt_atoms: u64) -> u32 {
    if principal_debt_atoms == 0 {
        return 0;
    }
    let ratio = (outstanding_debt_atoms as f64 / principal_debt_atoms as f64) * 10_000.0;
    ratio.clamp(0.0, u32::MAX as f64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_loan_skips_prefilter() {
        assert!(static_accrued_ratio_bps(1_000_000, 1_000_000) < STATIC_LTV_PREFILTER_BPS);
    }

    #[test]
    fn ratio_at_105_percent_meets_threshold() {
        assert!(static_accrued_ratio_bps(1_050_000, 1_000_000) >= STATIC_LTV_PREFILTER_BPS);
    }

    #[test]
    fn ratio_zero_principal_safe() {
        assert_eq!(static_accrued_ratio_bps(100, 0), 0);
    }
}
