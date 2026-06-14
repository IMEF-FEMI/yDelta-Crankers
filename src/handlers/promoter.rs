//! `ProcessMatchedLoan` (tag 5). Cranker pays loan-PDA rent; the
//! program refunds it to `loan.created_by` at claim time.

use std::sync::Mutex;
use std::time::Instant;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::{process_matched_loan_instruction, VaultSettleAddrs};
use ydelta::state::vault::{
    global_vault_integration_account_pda, global_vault_pda, global_vault_signer_pda,
    global_vault_staging_pda,
};
use ydelta::validation::token_checkers::get_vault_address;
use ydelta::validation::{get_lender_integration_account_address, get_market_signer_address};

use crate::chain_reader::{MarketView, PendingMatchedLoan};

use super::{Handler, HandlerContext};

pub struct PromoterHandler {
    inflight: Mutex<std::collections::HashSet<(Pubkey, u64)>>,
}

impl PromoterHandler {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(Default::default()),
        }
    }
}

impl Default for PromoterHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Handler for PromoterHandler {
    fn name(&self) -> &'static str {
        "promoter"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        let t0 = Instant::now();
        let markets = ctx.chain.list_markets().await?;
        let mut total_pending = 0usize;
        let mut promoted = 0usize;

        for m in &markets {
            if m.is_paused {
                continue;
            }
            let market_pk = m.address;
            let pending = match ctx.chain.read_pending_matched_loans(&market_pk).await {
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
                    Ok(false) => {}
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
        self.inflight.lock().unwrap().insert((e.market, e.sequence))
    }

    fn release_inflight(&self, e: &PendingMatchedLoan) {
        self.inflight
            .lock()
            .unwrap()
            .remove(&(e.market, e.sequence));
    }

    async fn promote_one(
        &self,
        ctx: &HandlerContext,
        market: &MarketView,
        entry: &PendingMatchedLoan,
    ) -> Result<bool> {
        let market_pk = market.address;
        let debt_mint = market.debt_mint;

        let banks = ctx.cfg.banks_snapshot();
        let debt_bank = banks
            .get(&debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for debt mint {debt_mint}"))?
            .clone();

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        let vault_settle = self
            .assemble_vault_settle(ctx, &market_pk, entry, &debt_mint, &debt_bank)
            .await?;

        let ix = process_matched_loan_instruction(
            &market_pk,
            &payer_pk,
            &debt_bank.bank,
            &ctx.cfg.marginfi_program_id,
            entry.sequence,
            None,
            vault_settle,
        );

        // Productive-crank gate: the queue node is removed on-chain only
        // AFTER every check passes (net_principal>0, live seats, vault-settle
        // funding), so a node that can't yet promote (e.g. the lender vault
        // is momentarily underfunded) would otherwise be re-fired every
        // tick. A free sim of the exact ix lets us skip instead of firing a
        // doomed tx — and keeps the crank productive even if the send path
        // ever drops RPC preflight.
        let sim = ctx.rpc.simulate(vec![ix.clone()], &payer_pk).await?;
        if !sim.ok {
            tracing::debug!(
                market = %market_pk,
                seq = entry.sequence,
                error = ?sim.error,
                "process_matched_loan sim failed; skipping submit"
            );
            return Ok(false);
        }

        let sig = ctx
            .rpc
            .send_signed_labeled("process_matched_loan", vec![ix], &[&fee_payer])
            .await?;
        tracing::info!(
            market = %market_pk,
            seq = entry.sequence,
            loan_type = entry.loan_type,
            presettled = entry.is_vault_presettled(),
            sig = %sig,
            "matched loan promoted"
        );
        Ok(true)
    }

    /// `VaultSettleAddrs` required iff the node is a non-presettled
    /// Fixed match against a vault lender. P2Pool nodes have no vault
    /// lender; presettled nodes have already had atoms migrated by
    /// `ConvertP2PoolToFixed`.
    async fn assemble_vault_settle(
        &self,
        ctx: &HandlerContext,
        market: &Pubkey,
        entry: &PendingMatchedLoan,
        debt_mint: &Pubkey,
        debt_bank: &crate::bank_registry::BankInfo,
    ) -> Result<Option<VaultSettleAddrs>> {
        if entry.is_p2pool() || entry.is_vault_presettled() || !entry.has_vault_lender() {
            return Ok(None);
        }

        let market_data = ctx
            .rpc
            .get_account_data(market)
            .await?
            .ok_or_else(|| anyhow!("market {market} disappeared"))?;
        let seat = ctx
            .chain
            .read_seat_at(&market_data, entry.lender_seat_index)?;
        if seat.owner_kind != ydelta::state::OWNER_KIND_SUB_VAULT {
            tracing::warn!(
                market = %market,
                sequence = entry.sequence,
                owner_kind = seat.owner_kind,
                "lender seat owner_kind disagrees with VAULT_LENDER flag; skipping bundle"
            );
            return Ok(None);
        }
        self.build_vault_settle_addrs(ctx, market, &seat.owner, debt_mint, debt_bank)
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
        let (expected_vault, _) = global_vault_pda(&debt_bank.bank);
        if expected_vault != *global_vault {
            return Err(anyhow!(
                "vault {global_vault} doesn't match bank {} PDA {expected_vault}",
                debt_bank.bank
            ));
        }
        let (market_debt_vault, _) = get_vault_address(market, debt_mint);
        let (market_lender_integration_account, _) =
            get_lender_integration_account_address(market);
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
            // `load_vault_settle_accounts` reads exactly one oracle.
            debt_oracles: vec![debt_bank.primary_oracle()],
            debt_mint: *debt_mint,
            token_program: debt_bank.token_program,
            marginfi_group: ctx.cfg.marginfi_group,
            marginfi_program: ctx.cfg.marginfi_program_id,
        }))
    }
}
