//! `ProcessMatchedLoan` (tag 7) — promote queue nodes into LoanFixed PDAs.
//!
//! Reads `MarketFixed` accounts via RPC (the indexer doesn't expose the
//! MatchedLoan queue), walks the hypertree, and submits one ix per
//! pending node. Cranker pays loan-PDA rent; recovered at claim time
//! when the lender (or our claimer handler) passes us as `cranker_refund`.
//!
//! Modes (driven by `MatchedLoan.flags`):
//!   - Primary       → empty loan PDA, builds via `process_matched_loan_instruction`
//!   - SecondaryFull → existing loan, builds via `process_secondary_matched_loan_instruction`
//!   - SecondarySplit → existing loan + new sub-loan PDA, builds via
//!     `process_secondary_split_matched_loan_instruction`
//!
//! Vault-lender primary matches additionally need the `VaultSettleAddrs`
//! bundle (15 accounts). Assembled from `BankRegistry` + on-chain
//! ydelta PDAs. Skipped with a warning if the bank isn't configured.

use std::sync::Mutex;
use std::time::Instant;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::{
    process_matched_loan_instruction, process_secondary_matched_loan_instruction,
    process_secondary_split_matched_loan_instruction, VaultSettleAddrs,
};
use ydelta::state::vault::{
    global_vault_integration_account_pda, global_vault_pda, global_vault_signer_pda,
    global_vault_staging_pda,
};
use ydelta::validation::token_checkers::get_vault_address;
use ydelta::validation::{get_lender_integration_account_address, get_market_signer_address};

use crate::indexer_client::MarketSummary;
use crate::market_reader::{read_pending_matched_loans, read_seat_at, PendingMatchedLoan};
use ydelta::state::OWNER_KIND_RISK_PROFILE;

use super::{Handler, HandlerContext};

pub struct PromoterHandler {
    /// Track in-flight (market, sequence) pairs so we don't double-submit
    /// when a tx is confirming. Reset on success/failure.
    inflight: Mutex<std::collections::HashSet<(Pubkey, u64)>>,
}

impl PromoterHandler {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(Default::default()),
        }
    }
}

#[async_trait]
impl Handler for PromoterHandler {
    fn name(&self) -> &'static str {
        "promoter"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        let t0 = Instant::now();
        let markets = ctx.indexer.markets().await?;
        let mut total_pending = 0usize;
        let mut promoted = 0usize;

        for m in &markets {
            if m.is_paused {
                continue;
            }
            let market_pk = m.address.parse::<Pubkey>()?;
            let pending = match read_pending_matched_loans(&ctx.rpc, &market_pk).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(market = %market_pk, error = %e, "read pending failed");
                    continue;
                }
            };
            total_pending += pending.len();
            for entry in pending {
                if !self.claim_inflight(&entry) {
                    continue;
                }
                let res = self.promote_one(ctx, m, &entry).await;
                self.release_inflight(&entry);
                match res {
                    Ok(true) => promoted += 1,
                    Ok(false) => {} // skipped (e.g. unsupported config)
                    Err(e) => tracing::warn!(
                        market = %entry.market,
                        seq = entry.sequence,
                        error = %e,
                        "promote failed"
                    ),
                }
            }
        }

        if total_pending > 0 {
            tracing::info!(
                pending = total_pending,
                promoted,
                elapsed_ms = t0.elapsed().as_millis() as u64,
                "promoter tick"
            );
        }
        Ok(())
    }
}

impl PromoterHandler {
    fn claim_inflight(&self, e: &PendingMatchedLoan) -> bool {
        let key = (e.market, e.sequence);
        let mut s = self.inflight.lock().unwrap();
        s.insert(key)
    }

    fn release_inflight(&self, e: &PendingMatchedLoan) {
        let mut s = self.inflight.lock().unwrap();
        s.remove(&(e.market, e.sequence));
    }

    async fn promote_one(
        &self,
        ctx: &HandlerContext,
        market: &MarketSummary,
        entry: &PendingMatchedLoan,
    ) -> Result<bool> {
        let market_pk: Pubkey = market.address.parse()?;
        let debt_mint: Pubkey = market.debt_mint.parse()?;
        let collateral_mint: Pubkey = market.collateral_mint.parse()?;

        let debt_bank = ctx
            .cfg
            .banks
            .get(&debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for debt mint {debt_mint}"))?;

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        let ix = if entry.is_secondary() {
            // Secondary cross: existing loan being transferred (or split).
            // The processor needs collateral bank/oracles for the LTV
            // re-check, plus optional vault_settle when the NEW lender
            // (taker) is a risk profile.
            let collateral_bank =
                ctx.cfg.banks.get(&collateral_mint).ok_or_else(|| {
                    anyhow!("no BANKS config for collateral mint {collateral_mint}")
                })?;

            if entry.is_split() {
                // Split sub-loan path. No vault_settle support yet — the
                // on-chain processor rejects vault new-lender + split
                // (`secondary cross to risk-profile new lender does not
                // yet support split — full-transfer only in v1`). If the
                // new lender is a vault, this match isn't promotable.
                if self.new_lender_is_vault(ctx, &market_pk, entry).await? {
                    tracing::warn!(
                        market = %market_pk,
                        seq = entry.sequence,
                        "skipping split secondary to vault new-lender (on-chain v1 restriction)"
                    );
                    return Ok(false);
                }
                process_secondary_split_matched_loan_instruction(
                    &market_pk,
                    &payer_pk,
                    /*queue_sequence=*/ entry.sequence,
                    /*referenced_loan_sequence=*/ entry.referenced_loan_sequence,
                    /*_next_market_sequence=*/ 0,
                    &debt_bank.bank,
                    &collateral_bank.bank,
                    &debt_bank.oracles,
                    &collateral_bank.oracles,
                    &ctx.cfg.marginfi_program_id,
                    None,
                )
            } else {
                // Full-transfer secondary. May need vault_settle if the
                // new lender (buyer) is a risk profile.
                let vault_settle = self
                    .assemble_secondary_vault_settle(ctx, &market_pk, entry, &debt_mint, debt_bank)
                    .await?;
                process_secondary_matched_loan_instruction(
                    &market_pk,
                    &payer_pk,
                    /*queue_sequence=*/ entry.sequence,
                    /*referenced_loan_sequence=*/ entry.referenced_loan_sequence,
                    &debt_bank.bank,
                    &collateral_bank.bank,
                    &debt_bank.oracles,
                    &collateral_bank.oracles,
                    &ctx.cfg.marginfi_program_id,
                    None,
                    vault_settle,
                )
            }
        } else {
            // Primary mode. Determine lender_kind from the seat; if vault,
            // assemble VaultSettleAddrs.
            let vault_settle = self
                .assemble_vault_settle(ctx, &market_pk, entry, &debt_mint, debt_bank)
                .await?;

            process_matched_loan_instruction(
                &market_pk,
                &payer_pk,
                &debt_bank.bank,
                &ctx.cfg.marginfi_program_id,
                entry.sequence,
                None,
                vault_settle,
            )
        };

        let sig = ctx
            .rpc
            .send_signed_labeled("process_matched_loan", vec![ix], &[&fee_payer])
            .await?;
        tracing::info!(
            market = %market_pk,
            seq = entry.sequence,
            secondary = entry.is_secondary(),
            split = entry.is_split(),
            sig = %sig,
            "matched loan promoted"
        );
        Ok(true)
    }

    /// For a secondary full-transfer, peek at the new lender's seat to
    /// decide whether the buyer is a risk profile. If yes, the
    /// processor requires `VaultSettleAddrs` to migrate the buyer's
    /// cash from `vault.integration` into `market.lender_integration`.
    async fn assemble_secondary_vault_settle(
        &self,
        ctx: &HandlerContext,
        market: &Pubkey,
        entry: &PendingMatchedLoan,
        debt_mint: &Pubkey,
        debt_bank: &crate::bank_registry::BankInfo,
    ) -> Result<Option<VaultSettleAddrs>> {
        if entry.new_lender_seat_index == hypertree::NIL {
            return Ok(None);
        }
        let market_data = ctx
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} disappeared"))?;
        let new_lender_seat = read_seat_at(&market_data, entry.new_lender_seat_index)?;
        if new_lender_seat.owner_kind != OWNER_KIND_RISK_PROFILE {
            return Ok(None);
        }
        let global_vault = new_lender_seat.owner;
        self.build_vault_settle_addrs(ctx, market, &global_vault, debt_mint, debt_bank)
    }

    async fn new_lender_is_vault(
        &self,
        ctx: &HandlerContext,
        market: &Pubkey,
        entry: &PendingMatchedLoan,
    ) -> Result<bool> {
        if entry.new_lender_seat_index == hypertree::NIL {
            return Ok(false);
        }
        let market_data = ctx
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} disappeared"))?;
        let seat = read_seat_at(&market_data, entry.new_lender_seat_index)?;
        Ok(seat.owner_kind == OWNER_KIND_RISK_PROFILE)
    }

    /// If the lender seat is a risk-profile, build VaultSettleAddrs from
    /// PDAs + the bank registry. Wallet lenders return Ok(None).
    async fn assemble_vault_settle(
        &self,
        ctx: &HandlerContext,
        market: &Pubkey,
        entry: &PendingMatchedLoan,
        debt_mint: &Pubkey,
        debt_bank: &crate::bank_registry::BankInfo,
    ) -> Result<Option<VaultSettleAddrs>> {
        // The fast path doesn't read the seat unless the flags hint at a
        // vault lender. The on-chain ix re-checks anyway, so a missed
        // hint just means we'd send the wrong-shape ix and revert. Use
        // the flag as the gate.
        if !entry.has_vault_lender() {
            return Ok(None);
        }

        // Get vault and profile from the seat referenced by `lender_seat_index`.
        let market_data = ctx
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} disappeared"))?;
        let seat = read_seat_at(&market_data, entry.lender_seat_index)?;
        let global_vault = seat.owner;
        self.build_vault_settle_addrs(ctx, market, &global_vault, debt_mint, debt_bank)
    }

    fn build_vault_settle_addrs(
        &self,
        ctx: &HandlerContext,
        market: &Pubkey,
        global_vault: &Pubkey,
        debt_mint: &Pubkey,
        debt_bank: &crate::bank_registry::BankInfo,
    ) -> Result<Option<VaultSettleAddrs>> {
        let (global_vault_signer, _) = global_vault_signer_pda(global_vault);
        let (global_vault_staging, _) = global_vault_staging_pda(global_vault);
        let (global_vault_integration_account, _) =
            global_vault_integration_account_pda(global_vault);
        let (expected_vault, _) = global_vault_pda(debt_mint);
        if expected_vault != *global_vault {
            return Err(anyhow!(
                "vault {global_vault} doesn't match debt_mint {debt_mint} PDA {expected_vault}"
            ));
        }
        let (market_debt_vault, _) = get_vault_address(market, debt_mint);
        let (market_lender_integration_account, _) = get_lender_integration_account_address(market);
        let (market_signer, _) = get_market_signer_address(market);

        Ok(Some(VaultSettleAddrs {
            global_vault: *global_vault,
            global_vault_signer,
            global_vault_staging,
            global_vault_integration_account,
            market_debt_vault,
            market_lender_integration_account,
            market_signer,
            debt_liquidity_vault: debt_bank.liquidity_vault,
            debt_bank_liquidity_vault_authority: debt_bank.liquidity_vault_authority,
            // Vault-settle path uses primary oracle only per the on-chain
            // `load_vault_settle_accounts` loader.
            debt_oracles: vec![debt_bank.primary_oracle()],
            debt_mint: *debt_mint,
            token_program: debt_bank.token_program,
            marginfi_group: ctx.cfg.marginfi_group,
            marginfi_program: ctx.cfg.marginfi_program_id,
        }))
    }
}
