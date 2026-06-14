//! `MatchCrank` (tag 43, v1 D7/D8). Permissionless cranker for the
//! two-sided book: walks resting asks against resting bids and resolves
//! any fillable cross.
//!
//! Why a dedicated crank exists: in v1 a book can sit **crossed at rest**
//! — a resting bid whose rate crosses a resting ask but that didn't fill
//! at order time (the ask's sub-vault had no idle, an LTV gate failed,
//! etc.). Crossability then changes with NO order flow: a vault deposit
//! replenishes a sub-vault's idle, a repayment frees capacity, an oracle
//! move flips an LTV gate. Ask placements/re-syncs take on their own, but
//! none of those off-book events trigger a match — this crank is the
//! permissionless backstop that resolves them. No keeper fee; the
//! cranker pays only tx fees.
//!
//! Gate: we only crank a market whose best-bid rate ≥ best-ask rate (a
//! rate-crossable pair exists) and whose sim succeeds. The program runs
//! the full term / idle / LTV / owner-self-cross checks itself, so a
//! rate-crossed pair that's permanently blocked (e.g. owner self-cross)
//! still no-ops on-chain; the sim can't distinguish a fill from a no-op
//! without log parsing, so that residual is accepted as a bounded cost
//! (throttled by `MATCH_CRANKER_INTERVAL_SEC`). A future optimisation is
//! to parse the `MatchCrankLog.fills` out of the sim logs and skip the
//! submit when zero.

use std::collections::HashSet;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount, instruction::Instruction,
    signature::Signer as _,
};
// NB: ydelta's `instruction_builders/mod.rs` declares `pub mod
// match_crank_instruction` but (unlike every other builder) omits the
// `pub use match_crank_instruction::*;` re-export, so we reach the fn via
// its submodule path. Harmless; flagged upstream as a re-export gap.
use ydelta::program::instruction_builders::match_crank_instruction::match_crank_instruction;

use base64::Engine as _;
use ydelta::logs::{Discriminant as _, MatchCrankLog};

use crate::chain_reader::MarketView;

use super::{Handler, HandlerContext};

pub struct MatchCrankerHandler {
    /// Per-market in-flight guard so a slow crank can't be double-issued
    /// within a tick.
    inflight: Mutex<HashSet<Pubkey>>,
}

impl MatchCrankerHandler {
    pub fn new() -> Self {
        Self {
            inflight: Mutex::new(HashSet::new()),
        }
    }
}

impl Default for MatchCrankerHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Handler for MatchCrankerHandler {
    fn name(&self) -> &'static str {
        "match_cranker"
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        let markets = ctx.chain.list_markets().await?;
        let mut cranked = 0usize;
        for market in &markets {
            if market.is_paused {
                continue;
            }
            match self.scan_market(ctx, market).await {
                Ok(true) => cranked += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(market = %market.address, error = %e, "match-crank scan failed")
                }
            }
        }
        if cranked > 0 {
            tracing::info!(cranked, "match_cranker tick");
        }
        Ok(())
    }
}

impl MatchCrankerHandler {
    async fn scan_market(&self, ctx: &HandlerContext, market: &MarketView) -> Result<bool> {
        if !ctx.chain.market_is_rate_crossed(&market.address).await? {
            return Ok(false);
        }
        if !self.claim_inflight(market.address) {
            return Ok(false);
        }
        let res = self.crank_one(ctx, market).await;
        self.release_inflight(market.address);
        res
    }

    fn claim_inflight(&self, m: Pubkey) -> bool {
        self.inflight.lock().unwrap().insert(m)
    }
    fn release_inflight(&self, m: Pubkey) {
        self.inflight.lock().unwrap().remove(&m);
    }

    async fn crank_one(&self, ctx: &HandlerContext, market: &MarketView) -> Result<bool> {
        let banks = ctx.cfg.banks_snapshot();
        let debt_bank = banks
            .get(&market.debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for debt mint {}", market.debt_mint))?
            .clone();
        let collateral_bank = banks
            .get(&market.collateral_mint)
            .ok_or_else(|| {
                anyhow!("no BANKS config for collateral mint {}", market.collateral_mint)
            })?
            .clone();

        let fee_payer = ctx.signers.fee_payer.clone();
        let payer_pk = fee_payer.pubkey();

        // The per-cross LTV gate inside match_crank reads BOTH bank
        // oracles; bundle a fresh Switchboard update in front of the sim
        // AND the real submit so pull feeds are current. Pyth-push stays
        // fresh via Pyth-DA, so it needs no crank here.
        let (swb_ixs, swb_luts) = self.fetch_swb_bundle(ctx, &debt_bank, &collateral_bank).await;

        // match_crank's first `bank` arg keys the vault PDA (`[b"vault",
        // bank]`) — that's the DEBT bank, same value passed as `debt_bank`.
        let ix = match_crank_instruction(
            &debt_bank.bank,
            &market.address,
            &payer_pk,
            &debt_bank.bank,
            &ctx.cfg.marginfi_group,
            &collateral_bank.bank,
            &debt_bank.oracles,
            &collateral_bank.oracles,
            ctx.cfg.handlers.match_cranker_max_fills,
        );

        // Sim first (free, with the SWB prepend): a degenerate/stale
        // oracle or any other revert never costs SOL.
        let mut sim_bundle = swb_ixs.clone();
        sim_bundle.push(ix.clone());
        let sim = ctx.rpc.simulate_v0(sim_bundle, &swb_luts, &payer_pk).await?;
        if !sim.ok {
            tracing::debug!(market = %market.address, error = ?sim.error, "match-crank sim failed");
            return Ok(false);
        }

        // Productive-crank gate. A rate-crossed pair can still be unfillable
        // (sub-vault idle==0, sunset, term mismatch, owner self-cross,
        // LTV-cap) — and a 0-fill MatchCrank SUCCEEDS on-chain (emits
        // MatchCrankLog{fills:0}, returns Ok), so `sim.ok` alone can't tell
        // it apart from a real fill. On a Switchboard-collateral market the
        // bundled oracle update would then LAND and pay its per-update
        // charge for nothing. Parse the sim's MatchCrankLog and only submit
        // when it reports a real fill.
        match parse_match_crank_fills(&sim.logs) {
            Some(0) => {
                tracing::debug!(market = %market.address, "match-crank sim: 0 fills; skipping submit");
                return Ok(false);
            }
            Some(fills) => {
                tracing::debug!(market = %market.address, fills, "match-crank sim: fills > 0; submitting");
            }
            None => {
                // MatchCrankLog is emitted unconditionally, so a missing
                // line most likely means a high-fill tx whose logs were
                // truncated by the runtime — i.e. a productive crank. Fail
                // OPEN (submit) to preserve matching liveness, but surface it.
                tracing::warn!(market = %market.address, "match-crank sim: no parseable MatchCrankLog; submitting (fail-open)");
            }
        }

        let mut real_bundle = swb_ixs;
        real_bundle.push(ix);
        let sig = ctx
            .rpc
            .send_signed_v0_labeled("match_crank", real_bundle, &swb_luts, &[&fee_payer])
            .await?;
        tracing::info!(market = %market.address, sig = %sig, "match crank submitted");
        Ok(true)
    }

    /// Build (don't submit) the Switchboard fetch-update bundle for any
    /// pull-feed bank in the pair, so the caller can prepend it to both
    /// the sim and the real submit. Empty when neither side is
    /// Switchboard or no cranker is configured; a gateway failure is
    /// non-fatal (the on-chain staleness gate decides).
    async fn fetch_swb_bundle(
        &self,
        ctx: &HandlerContext,
        debt_bank: &crate::bank_registry::BankInfo,
        collateral_bank: &crate::bank_registry::BankInfo,
    ) -> (Vec<Instruction>, Vec<AddressLookupTableAccount>) {
        let Some(cranker) = ctx.swb_cranker.as_ref() else {
            return (vec![], vec![]);
        };
        let mut feeds = Vec::new();
        if debt_bank.is_switchboard_pull() {
            feeds.push(debt_bank.primary_oracle());
        }
        if collateral_bank.is_switchboard_pull() {
            feeds.push(collateral_bank.primary_oracle());
        }
        if feeds.is_empty() {
            return (vec![], vec![]);
        }
        match cranker.fetch_update_ixs(feeds).await {
            Ok(bundle) => bundle,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "switchboard gateway fetch failed; proceeding without prepend (sim will gate)"
                );
                (vec![], vec![])
            }
        }
    }
}

/// Pull the fill count out of a `MatchCrank` simulation's logs.
///
/// `MatchCrankLog` is emitted via `sol_log_data`, which surfaces as a
/// `Program data: <base64>` line. `emit_stack` lays it out on the wire as
/// `[discriminant(8) | market(32) | cranker(32) | fills:u32 | pad(4)]`
/// (80 bytes), so `fills` is a little-endian u32 at byte offset 72. The
/// discriminant is keccak(program_id ‖ "MatchCrankLog"), computed against
/// the linked ydelta's id so it matches the on-chain emitter. Returns
/// `None` when no MatchCrankLog line is present (e.g. log truncation).
fn parse_match_crank_fills(logs: &[String]) -> Option<u32> {
    let disc = MatchCrankLog::discriminant();
    for line in logs {
        let Some(rest) = line.strip_prefix("Program data: ") else {
            continue;
        };
        // sol_log_data emits space-separated base64 chunks; emit_stack
        // passes a single slice, so the first token is the whole record.
        let token = rest.split_whitespace().next().unwrap_or(rest);
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(token) else {
            continue;
        };
        if bytes.len() < 80 || bytes[..8] != disc {
            continue;
        }
        return Some(u32::from_le_bytes(bytes[72..76].try_into().ok()?));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the wire form `emit_stack` produces for a MatchCrankLog:
    /// `Program data: base64([disc | market | cranker | fills | pad])`.
    fn match_crank_log_line(fills: u32) -> String {
        let mut rec = Vec::with_capacity(80);
        rec.extend_from_slice(&MatchCrankLog::discriminant());
        rec.extend_from_slice(&[0u8; 32]); // market
        rec.extend_from_slice(&[0u8; 32]); // cranker
        rec.extend_from_slice(&fills.to_le_bytes());
        rec.extend_from_slice(&[0u8; 4]); // pad
        format!(
            "Program data: {}",
            base64::engine::general_purpose::STANDARD.encode(&rec)
        )
    }

    #[test]
    fn parses_zero_fills() {
        let logs = vec![
            "Program invoke [1]".to_string(),
            match_crank_log_line(0),
            "Program success".to_string(),
        ];
        assert_eq!(parse_match_crank_fills(&logs), Some(0));
    }

    #[test]
    fn parses_nonzero_fills() {
        let logs = vec![match_crank_log_line(3)];
        assert_eq!(parse_match_crank_fills(&logs), Some(3));
    }

    #[test]
    fn ignores_unrelated_program_data() {
        // A different event's Program-data line (wrong discriminant) must
        // not be mistaken for a MatchCrankLog.
        let other = {
            let mut rec = vec![0xAAu8; 80];
            // ensure first 8 bytes differ from the real discriminant
            rec[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
            format!(
                "Program data: {}",
                base64::engine::general_purpose::STANDARD.encode(&rec)
            )
        };
        assert_eq!(parse_match_crank_fills(&[other]), None);
    }

    #[test]
    fn none_when_absent() {
        let logs = vec!["Program log: nothing here".to_string()];
        assert_eq!(parse_match_crank_fills(&logs), None);
    }
}
