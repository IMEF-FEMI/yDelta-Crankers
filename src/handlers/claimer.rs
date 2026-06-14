//! `ClaimRepaymentForSubVault` (tag 20). Stateless per-(sub_vault, market)
//! sweeper. The on-chain processor moves `debt_withdrawable_shares` from
//! the per-market `lender_marginfi_account` into the per-vault marginfi
//! integration account, then decrements both the seat shares and
//! `sub_vault.pending_claim_atoms`. Repay / liquidate / settle do their own
//! close-out + loan-PDA close in-tx; the cranker just sweeps leftover
//! seat shares.

use std::{collections::HashSet, sync::Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::signature::Signer as _;
use ydelta::program::instruction_builders::claim_repayment_for_sub_vault_instruction;
use ydelta::validation::get_lender_integration_account_address;

use crate::chain_reader::PendingVaultClaim;

use super::{Handler, HandlerContext};

pub struct ClaimerHandler {
    /// Deduplicates concurrent sweep attempts within a tick by (market, vault, sub_vault_id).
    inflight: Mutex<HashSet<(Pubkey, Pubkey, u16)>>,
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
        // Atom-denominated gate FIRST. A claim only exists once a repay /
        // settle_matured_loan / liquidate_loan has retired atoms into a
        // sub_vault's `SubVault.pending_claim_atoms` (the only writers of
        // that field; the claim ix is the only reader that decrements it).
        // Reading the sub-vaults is a single getProgramAccounts, so when
        // nothing is pending we skip the per-market seat-tree scan AND any
        // tx entirely — that's the "is there actually something to claim
        // on-chain" check. Crucially `pending_claim_atoms` is in ATOMS, so a
        // seat left holding sub-atom dust shares (which the marginfi
        // withdraw rounds to 0) does NOT keep us cranking once the realized
        // atoms are gone.
        let sub_vaults = ctx.chain.list_sub_vaults().await?;
        let claimable: HashSet<(Pubkey, u16)> = sub_vaults
            .iter()
            .filter(|p| p.pending_claim_atoms > 0)
            .map(|p| (p.vault, p.sub_vault_id))
            .collect();
        if claimable.is_empty() {
            return Ok(());
        }

        let markets = ctx.chain.list_markets().await?;
        if markets.is_empty() {
            return Ok(());
        }
        // `pending_claim_atoms` is a per-sub_vault aggregate across markets;
        // it can't say WHICH market holds the shares. The ClaimedSeat tree
        // is the authoritative per-market router, so scan it and keep only
        // seats whose sub_vault has real atoms awaiting a sweep.
        let pending: Vec<_> = ctx
            .chain
            .list_pending_vault_claims(&markets)
            .await?
            .into_iter()
            .filter(|e| claimable.contains(&(e.lender_global_vault, e.sub_vault_id)))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }

        // Dust seats (a marginfi withdraw that rounds to 0 atoms) need no
        // client-side guard: the program self-heals on the first sweep —
        // it zeroes the seat when 0 shares burn, or shrinks it otherwise —
        // so a dust seat clears in one productive tx and never re-appears
        // unchanged. The `pending_claim_atoms` gate above already blocks the
        // tx entirely once the realized atoms are gone.
        let mut claimed = 0;
        for entry in &pending {
            let key = (entry.market, entry.lender_global_vault, entry.sub_vault_id);
            if !self.claim_inflight(key) {
                continue;
            }
            let res = self.sweep_one(ctx, entry).await;
            self.release_inflight(key);
            match res {
                Ok(true) => claimed += 1,
                // Sim said the sweep would revert (stale oracle, health
                // check) — skip; it retries for free next tick once the
                // rejecting condition clears.
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    market = %entry.market,
                    vault = %entry.lender_global_vault,
                    sub_vault_id = entry.sub_vault_id,
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
    fn claim_inflight(&self, key: (Pubkey, Pubkey, u16)) -> bool {
        self.inflight.lock().unwrap().insert(key)
    }

    fn release_inflight(&self, key: (Pubkey, Pubkey, u16)) {
        self.inflight.lock().unwrap().remove(&key);
    }

    /// Returns `Ok(true)` when a sweep tx was submitted, `Ok(false)` when
    /// the pre-send sim showed it would do nothing / revert (skip).
    async fn sweep_one(
        &self,
        ctx: &HandlerContext,
        entry: &PendingVaultClaim,
    ) -> Result<bool> {
        let banks = ctx.cfg.banks_snapshot();
        let debt_bank = banks
            .get(&entry.debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for debt mint {}", entry.debt_mint))?
            .clone();

        let (lender_marginfi, _) = get_lender_integration_account_address(&entry.market);

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        let ix = claim_repayment_for_sub_vault_instruction(
            &payer_pk,
            &entry.market,
            entry.sub_vault_id,
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

        // Productive-crank gate: nothing off-chain distinguishes a sweep
        // that moves atoms from one the program rejects (stale oracle,
        // marginfi health check). A free sim of the exact ix lets us skip
        // instead of landing a revert — and survives a future skip_preflight.
        // The dust self-heal sweep still sims OK (zeroing the seat is a real
        // state change), so it is submitted once and then disappears.
        let sim = ctx.rpc.simulate(vec![ix.clone()], &payer_pk).await?;
        if !sim.ok {
            tracing::debug!(
                market = %entry.market,
                vault = %entry.lender_global_vault,
                sub_vault_id = entry.sub_vault_id,
                error = ?sim.error,
                "claim_repayment sim failed; skipping submit"
            );
            return Ok(false);
        }

        let sig = ctx
            .rpc
            .send_signed_labeled("claim_repayment_for_sub_vault", vec![ix], &[&fee_payer])
            .await?;
        tracing::info!(
            market = %entry.market,
            vault = %entry.lender_global_vault,
            sub_vault_id = entry.sub_vault_id,
            shares = entry.debt_withdrawable_shares,
            sig = %sig,
            "vault claim swept"
        );
        Ok(true)
    }
}
