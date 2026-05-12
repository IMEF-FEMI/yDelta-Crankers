//! `SettleMaturedLoan` (tag 20) + `LiquidateLoan` (tag 21).
//!
//! Discovery: indexer `/v1/loans?state=active` for each market we know
//! about. For each loan:
//!   - Compute the static (oracle-free) collateral/debt ratio and skip
//!     obviously-overcollateralized loans to save sim cost.
//!   - Pre-flight via `CheckMaturityLiquidatable` (tag 41) or
//!     `CheckLtvLiquidatable` (tag 40) via `simulateTransaction`.
//!   - On Ok sim, send the real ix.
//!
//! Profit gate: `MIN_LIQUIDATION_PROFIT_ATOMS` filters out loans where
//! the keeper bonus is below tx-fee threshold.
//!
//! Required external state:
//!   - Fee-payer's debt-mint ATA must hold enough debt asset to repay
//!     the loan. ATA address is derived from `(fee_payer, debt_mint,
//!     token_program)` via `BankInfo::ata_for` — no env config.
//!   - Bank info for both mints (auto-discovered at boot from each
//!     market's `MarketFixed` account; see `bank_registry.rs`).

use std::{collections::HashSet, str::FromStr, sync::Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::{
    check_ltv_liquidatable_instruction, check_maturity_liquidatable_instruction,
    liquidate_loan_instruction, settle_matured_loan_instruction,
};

use crate::indexer_client::{LoanSummary, LoansQuery, MarketSummary};

use super::util::now_unix;
use super::{Handler, HandlerContext};

const LOAN_STATE_ACTIVE: i16 = 0;
/// Static LTV pre-filter — only loans above this static debt/collateral
/// ratio (atom-based, no oracle adjustment) are worth simulating.
/// Generous floor; the real LTV gate happens in the sim.
const STATIC_LTV_PREFILTER_BPS: u32 = 5_000;

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

#[async_trait]
impl Handler for LiquidatorHandler {
    fn name(&self) -> &'static str {
        "liquidator"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        let markets = ctx.indexer.markets().await?;
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
    async fn scan_market(&self, ctx: &HandlerContext, market: &MarketSummary) -> Result<usize> {
        let market_pk = Pubkey::from_str(&market.address)?;
        let loans = ctx
            .indexer
            .loans(LoansQuery {
                market: Some(&market_pk),
                state: Some("active"),
                limit: Some(500),
                ..Default::default()
            })
            .await?;

        let t_now = now_unix();
        let grace_buffer = ctx.cfg.thresholds.maturity_extra_buffer.as_secs() as i64;

        let mut count = 0;
        for loan in &loans {
            if loan.state != LOAN_STATE_ACTIVE {
                continue;
            }
            if loan.outstanding_debt_atoms <= 0 {
                continue;
            }
            // Cheap static prefilter — don't pay for sims on obviously
            // healthy loans.
            let static_ltv_bps =
                static_ltv_bps(loan.outstanding_debt_atoms, loan.principal_debt_atoms);

            let matured = t_now >= loan.matures_at_unix + grace_buffer;
            let maybe_ltv_breach = static_ltv_bps >= STATIC_LTV_PREFILTER_BPS;

            if !matured && !maybe_ltv_breach {
                continue;
            }

            let loan_pk = Pubkey::from_str(&loan.address)?;
            if !self.claim_inflight(loan_pk) {
                continue;
            }
            let res = if matured {
                self.try_settle_matured(ctx, market, loan).await
            } else {
                self.try_liquidate_ltv(ctx, market, loan).await
            };
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
        let mut s = self.inflight.lock().unwrap();
        s.insert(loan)
    }
    fn release_inflight(&self, loan: Pubkey) {
        let mut s = self.inflight.lock().unwrap();
        s.remove(&loan);
    }

    async fn try_settle_matured(
        &self,
        ctx: &HandlerContext,
        market: &MarketSummary,
        loan: &LoanSummary,
    ) -> Result<bool> {
        let market_pk = Pubkey::from_str(&market.address)?;
        let debt_mint = Pubkey::from_str(&market.debt_mint)?;
        let collateral_mint = Pubkey::from_str(&market.collateral_mint)?;

        let (debt_bank, collateral_bank) = self.banks(ctx, &debt_mint, &collateral_mint)?;

        let full = ctx.indexer.loan(&Pubkey::from_str(&loan.address)?).await?;
        let sequence = u64::try_from(full.matched_loan_sequence)?;

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        // Pre-flight via tag 41.
        let sim_ix = check_maturity_liquidatable_instruction(
            &market_pk,
            &payer_pk,
            sequence,
            &debt_bank.bank,
            &ctx.cfg.marginfi_program_id,
        );
        let _ = &debt_bank.oracles; // unused on the maturity-only sim path
        let sim = ctx.rpc.simulate(vec![sim_ix], &payer_pk).await?;
        if !sim.ok {
            tracing::debug!(loan = %loan.address, error = ?sim.error, "maturity sim failed");
            return Ok(false);
        }

        // ATAs are deterministic PDAs of `(owner, token_program, mint)`.
        // We derive them from the bank metadata; no env input. The
        // accounts themselves must exist on chain (fund/create them
        // once via the wallet's normal SPL flow).
        let liquidator_debt_ata = debt_bank.ata_for(&payer_pk);
        let liquidator_collateral_ata = collateral_bank.ata_for(&payer_pk);

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
            // The processor handles both debt + collateral atom flows under
            // the debt mint's token program in v0.1.8 of the program
            // (single token_program account). If we ever face mixed
            // legacy/2022 pairs in a market, the on-chain ix would need
            // an extra account; for now we use the debt-side program.
            &debt_bank.token_program,
            &ctx.cfg.marginfi_group,
            &ctx.cfg.marginfi_program_id,
            0, // 0 = full repay
        );

        let sig = ctx
            .rpc
            .send_signed_labeled("settle_matured_loan", vec![ix], &[&fee_payer])
            .await?;
        tracing::info!(loan = %loan.address, sig = %sig, "matured loan settled");
        Ok(true)
    }

    async fn try_liquidate_ltv(
        &self,
        ctx: &HandlerContext,
        market: &MarketSummary,
        loan: &LoanSummary,
    ) -> Result<bool> {
        let market_pk = Pubkey::from_str(&market.address)?;
        let debt_mint = Pubkey::from_str(&market.debt_mint)?;
        let collateral_mint = Pubkey::from_str(&market.collateral_mint)?;

        let (debt_bank, collateral_bank) = self.banks(ctx, &debt_mint, &collateral_mint)?;

        let full = ctx.indexer.loan(&Pubkey::from_str(&loan.address)?).await?;
        let sequence = u64::try_from(full.matched_loan_sequence)?;

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        // Pre-flight via tag 40.
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
        let sim = ctx.rpc.simulate(vec![sim_ix], &payer_pk).await?;
        if !sim.ok {
            tracing::debug!(loan = %loan.address, error = ?sim.error, "ltv sim failed");
            return Ok(false);
        }

        // Profit gate (heuristic — refine once we read fee_config bonus
        // bps from indexer).
        if (loan.outstanding_debt_atoms as u64) < ctx.cfg.thresholds.min_liquidation_profit_atoms {
            tracing::debug!(loan = %loan.address, "below min liquidation profit");
            return Ok(false);
        }

        // ATAs are deterministic PDAs of `(owner, token_program, mint)`.
        // We derive them from the bank metadata; no env input.
        let liquidator_debt_ata = debt_bank.ata_for(&payer_pk);
        let liquidator_collateral_ata = collateral_bank.ata_for(&payer_pk);

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
            0, // 0 = full repay
        );

        let sig = ctx
            .rpc
            .send_signed_labeled("liquidate_loan", vec![ix], &[&fee_payer])
            .await?;
        tracing::info!(loan = %loan.address, sig = %sig, "ltv-breach loan liquidated");
        Ok(true)
    }

    fn banks<'a>(
        &self,
        ctx: &'a HandlerContext,
        debt_mint: &Pubkey,
        collateral_mint: &Pubkey,
    ) -> Result<(
        &'a crate::bank_registry::BankInfo,
        &'a crate::bank_registry::BankInfo,
    )> {
        let debt_bank = ctx
            .cfg
            .banks
            .get(debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for debt mint {debt_mint}"))?;
        let collateral_bank = ctx
            .cfg
            .banks
            .get(collateral_mint)
            .ok_or_else(|| anyhow!("no BANKS config for collateral mint {collateral_mint}"))?;
        Ok((debt_bank, collateral_bank))
    }
}

/// Static debt/principal ratio in bps. Cheap pre-filter only — the real
/// LTV gate is oracle-priced and runs in the sim.
fn static_ltv_bps(outstanding_debt_atoms: i64, principal_debt_atoms: i64) -> u32 {
    if principal_debt_atoms <= 0 {
        return 0;
    }
    let ratio = (outstanding_debt_atoms as f64 / principal_debt_atoms as f64) * 10_000.0;
    ratio.clamp(0.0, u32::MAX as f64) as u32
}
