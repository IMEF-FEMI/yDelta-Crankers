//! `UpdateOrderForRiskProfile` (tag 18) + `SetSeatMaxExposureForRiskProfile`
//! (tag 42) — keeps each managed (vault, profile, market) ask aligned
//! with its configured target rate AND the profile's risk-weighted
//! exposure cap.
//!
//! Two modes per `CuratorSignerConfig.rate_target`:
//!
//!   - **Static**:  always quote `rate_bps`.
//!   - **Dynamic**: read the market's debt-mint marginfi bank, compute
//!     `target = supply_apr + α × (borrow_apr - supply_apr)`. Fall back
//!     to the configured `fallback_bps` if the bank read fails.
//!
//! Per-tick the keeper:
//!   1. Reads the live `RiskProfile` from the vault, derives a target
//!      `max_exposure_atoms` from `(max_ltv_bps, max_term_seconds,
//!      allowed_market_max) × max(profile.total_principal_atoms,
//!      cfg.exposure_baseline_atoms)`. More aggressive policies (higher
//!      LTV, longer terms, more markets) get a larger share of the
//!      pool; the bootstrap baseline keeps quotes bounded until real
//!      deposits arrive.
//!   2. Reads the on-chain seat's `max_exposure_atoms`. If the
//!      computed target diverges beyond
//!      `CURATOR_MIN_EXPOSURE_DELTA_BPS`, issues a
//!      `SetSeatMaxExposureForRiskProfile` ix to update the cap in
//!      place. Bundled with `UpdateOrderForRiskProfile` so the resting
//!      ask's `principal_atoms` (re-read from the seat at place/update
//!      time) reflects the new cap in a single tx.
//!   3. Otherwise reconciles only the rate via
//!      `UpdateOrderForRiskProfile` when the delta exceeds
//!      `CURATOR_MIN_DELTA_BPS`.

use std::{collections::HashMap, sync::Mutex, time::Instant};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use solana_program::pubkey::Pubkey;
use solana_sdk::instruction::Instruction;
use ydelta::program::instruction_builders::{
    claim_seat_for_risk_profile_instruction, place_order_for_risk_profile_instruction,
    set_seat_max_exposure_for_risk_profile_instruction, update_order_for_risk_profile_instruction,
};
use ydelta::state::OWNER_KIND_RISK_PROFILE;

use crate::chain_reader::RiskProfileView;
use crate::config::{CuratorSignerConfig, RateTarget};
use crate::marginfi_bank::BankView;
use crate::marginfi_rate::{compute_rates, target_rate_bps};

use super::{Handler, HandlerContext};

const SIDE_ASK: u8 = 1;

/// Risk-score weights (parts per 100). LTV dominates because it's the
/// most direct expression of per-loan risk tolerance; term and breadth
/// are secondary modulators. Tunable as-needed — keep them in the same
/// scale (sum to 100) so the weighted score stays in [0, 10_000] bps.
const RISK_WEIGHT_LTV: u32 = 50;
const RISK_WEIGHT_TERM: u32 = 25;
const RISK_WEIGHT_MARKETS: u32 = 25;
const RISK_WEIGHT_SUM: u32 = RISK_WEIGHT_LTV + RISK_WEIGHT_TERM + RISK_WEIGHT_MARKETS;

/// Normalization horizon for `max_term_seconds`. A profile that allows
/// year-long loans scores the max term-component; shorter profiles get
/// a proportionally smaller share.
const TERM_NORMALIZATION_SECS: u64 = 365 * 24 * 60 * 60;

/// Normalization cap for `allowed_market_max` — matches
/// `RiskProfile::ACTIVE_MARKETS_MAX` on chain.
const MARKETS_NORMALIZATION: u32 = 8;

pub struct CuratorKeeperHandler {
    /// `(global_vault, profile_id, market) → last update Instant`.
    last_update: Mutex<HashMap<(Pubkey, u8, Pubkey), Instant>>,
}

impl CuratorKeeperHandler {
    pub fn new() -> Self {
        Self {
            last_update: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Handler for CuratorKeeperHandler {
    fn name(&self) -> &'static str {
        "curator_keeper"
    }

    /// Curator keeper uses a co-signer pattern: the fee payer pays
    /// the tx fee (signers[0]) and the per-profile curator co-signs
    /// to satisfy the on-chain `signer == profile.curator` gate.
    /// Curator wallets only ever pay on-chain rent for vault/market
    /// dynamic-region growth — typically a few places' worth at
    /// setup, then ~0 once the free list cycles. The fee payer's SOL
    /// balance gates the handler so a drained fee payer pauses
    /// keeper activity instead of spewing rejected txs.
    fn requires_fee_payer(&self) -> bool {
        true
    }

    async fn tick(&self, ctx: &HandlerContext) -> Result<()> {
        for cfg in &ctx.cfg.curator_signers {
            for market in &cfg.managed_markets {
                if let Err(e) = self.maintain_one(ctx, cfg, market).await {
                    tracing::warn!(
                        vault = %cfg.global_vault,
                        profile_id = cfg.profile_id,
                        market = %market,
                        error = %e,
                        "maintain_one failed"
                    );
                }
            }
        }
        Ok(())
    }
}

impl CuratorKeeperHandler {
    async fn maintain_one(
        &self,
        ctx: &HandlerContext,
        cfg: &CuratorSignerConfig,
        market: &Pubkey,
    ) -> Result<()> {
        // Throttle by per-(profile, market) cooldown. Covers both
        // place and update actions — without this we'd reattempt
        // `PlaceOrderForRiskProfile` every tick on a profile that has
        // no live order *and* the profile-seat preconditions aren't
        // met yet (e.g. the vault hasn't claimed a market seat), which
        // would log-spam and burn priority fees.
        let key = (cfg.global_vault, cfg.profile_id, *market);
        if let Some(prev) = self.last_update.lock().unwrap().get(&key) {
            if prev.elapsed() < ctx.cfg.thresholds.curator_min_update_interval {
                return Ok(());
            }
        }

        // Look up the live order by walking the market's order book on
        // chain. The vault-owned ask we're managing is identified by
        // `(owner_kind = RISK_PROFILE, owner = vault, profile_id, side
        // = Ask)`. `None` here is the "bootstrap" case — we'll place
        // the initial order rather than wait for manual seeding.
        let orders = ctx.chain.list_market_orders(market).await?;
        let live = orders.iter().find(|o| {
            o.owner_kind == OWNER_KIND_RISK_PROFILE
                && o.risk_profile_id == cfg.profile_id
                && o.side == SIDE_ASK
                && o.owner == cfg.global_vault
        });

        let market_view = ctx
            .chain
            .list_markets()
            .await?
            .into_iter()
            .find(|m| m.address == *market)
            .ok_or_else(|| anyhow!("market {} not found on chain", market))?;
        let debt_mint = market_view.debt_mint;

        // Read the live `RiskProfile` so we can derive both the risk
        // score and the deposit-driven pool that backs the target
        // exposure. Missing here means the profile was wound down out
        // from under us — log and bail.
        let profile = ctx
            .chain
            .read_risk_profile(&cfg.global_vault, cfg.profile_id)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "risk_profile {}#{} not found on chain",
                    cfg.global_vault,
                    cfg.profile_id
                )
            })?;
        let target_exposure_atoms = compute_target_max_exposure_atoms(&profile, cfg);

        // Resolve the target rate. Used by both the place and update
        // paths; we compute it before the branch so the bootstrap
        // path uses the same logic.
        let target_bps = match cfg.rate_target {
            RateTarget::Static { rate_bps } => rate_bps,
            RateTarget::Dynamic {
                alpha_bps,
                fallback_bps,
            } => match self.dynamic_target(ctx, &debt_mint, alpha_bps).await {
                Ok(t) => {
                    tracing::debug!(
                        vault = %cfg.global_vault,
                        profile_id = cfg.profile_id,
                        market = %market,
                        alpha_bps,
                        dynamic_target_bps = t,
                        "dynamic rate computed"
                    );
                    t
                }
                Err(e) => {
                    tracing::warn!(
                        vault = %cfg.global_vault,
                        profile_id = cfg.profile_id,
                        market = %market,
                        error = %e,
                        "dynamic rate computation failed; falling back to {fallback_bps} bps"
                    );
                    fallback_bps
                }
            },
        };

        // Split-payer pattern: the risk-profile ixs all carry a
        // dedicated `fee_payer` slot (signers[0]) for the tx fee +
        // on-chain rent (any market/vault dynamic-region expansion),
        // and a separate `curator` slot (signers[1]) that satisfies
        // the on-chain `signer == profile.curator` auth gate. No
        // lamports are debited from the curator account — the
        // operator only ever has to fund the cranker's fee_payer.
        let fee_payer = ctx.signers.fee_payer.clone();
        let curator = ctx.signers.curator_for(cfg)?;
        use solana_sdk::signature::Signer as _;
        let fee_payer_pk = fee_payer.pubkey();
        let curator_pk = curator.pubkey();

        match live {
            None => {
                // Bootstrap: no resting ask for this (vault, profile)
                // in this market yet. Before placing, the program
                // requires a vault-owned `ClaimedSeat` for
                // `(vault, profile_id)` to exist in this market. If
                // it's missing, bundle claim+place into one atomic tx
                // — single tx fee, and the place doesn't have to
                // wait for the next 5-min tick.
                let seat = ctx
                    .chain
                    .read_vault_seat(market, &cfg.global_vault, cfg.profile_id)
                    .await?;
                let place_ix = place_order_for_risk_profile_instruction(
                    &debt_mint,
                    market,
                    &fee_payer_pk,
                    &curator_pk,
                    cfg.profile_id,
                    target_bps,
                    cfg.target_term_seconds,
                    0, // flags
                );

                let (sig, claimed) = if seat.is_none() {
                    // Both claim_seat and place_order are
                    // curator-gated (the ydelta program enforces
                    // `signer == profile.curator` on both). Same
                    // curator keypair signs both ixs in this bundle;
                    // fee_payer covers tx fee + rent expansion for
                    // the new seat / order blocks.
                    let claim_ix = claim_seat_for_risk_profile_instruction(
                        &debt_mint,
                        market,
                        &fee_payer_pk,
                        &curator_pk,
                        cfg.profile_id,
                        target_exposure_atoms,
                    );
                    let sig = ctx
                        .rpc
                        .send_signed_labeled(
                            "claim_seat_and_place_order",
                            vec![claim_ix, place_ix],
                            &[&fee_payer, &curator],
                        )
                        .await?;
                    (sig, true)
                } else {
                    // Seat already exists — its cap may be stale vs
                    // the freshly computed target. Reconcile in the
                    // same tx as the place so the resting ask's
                    // `principal_atoms` (read from seat at place
                    // time) reflects the current target.
                    let on_chain = seat.as_ref().map(|s| s.max_exposure_atoms).unwrap_or(0);
                    let mut ixs = Vec::with_capacity(2);
                    if should_resync_exposure(
                        on_chain,
                        target_exposure_atoms,
                        seat.as_ref().map(|s| s.deployed_atoms).unwrap_or(0),
                        ctx.cfg.thresholds.curator_min_exposure_delta_bps,
                    ) {
                        ixs.push(set_seat_max_exposure_for_risk_profile_instruction(
                            &debt_mint,
                            market,
                            &fee_payer_pk,
                            &curator_pk,
                            cfg.profile_id,
                            clamp_target_above_deployed(
                                target_exposure_atoms,
                                seat.as_ref().map(|s| s.deployed_atoms).unwrap_or(0),
                            ),
                        ));
                    }
                    ixs.push(place_ix);
                    let sig = ctx
                        .rpc
                        .send_signed_labeled(
                            "place_order_for_risk_profile",
                            ixs,
                            &[&fee_payer, &curator],
                        )
                        .await?;
                    (sig, false)
                };

                tracing::info!(
                    vault = %cfg.global_vault,
                    profile_id = cfg.profile_id,
                    market = %market,
                    rate_bps = target_bps,
                    term_seconds = cfg.target_term_seconds,
                    target_exposure_atoms,
                    seat_auto_claimed = claimed,
                    sig = %sig,
                    "curator order placed (bootstrap)"
                );
                self.last_update.lock().unwrap().insert(key, Instant::now());
            }
            Some(o) => {
                // Reconcile exposure and rate independently. The
                // resting order's `principal_atoms` is re-read from
                // the seat's `max_exposure_atoms` whenever
                // `update_order_for_risk_profile` fires (cancel +
                // replace), so any cap mutation needs to bundle with
                // an update_order to take effect on the visible
                // order. If only the rate moved, send just the
                // update_order.
                let seat = ctx
                    .chain
                    .read_vault_seat(market, &cfg.global_vault, cfg.profile_id)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "live order present without a matching ClaimedSeat — \
                             on-chain state is inconsistent"
                        )
                    })?;

                let exposure_drifted = should_resync_exposure(
                    seat.max_exposure_atoms,
                    target_exposure_atoms,
                    seat.deployed_atoms,
                    ctx.cfg.thresholds.curator_min_exposure_delta_bps,
                );

                let current_bps = o.rate_bps as i32;
                let delta = (target_bps as i32 - current_bps).unsigned_abs();
                let rate_drifted = delta >= ctx.cfg.thresholds.curator_min_delta_bps as u32;

                if !exposure_drifted && !rate_drifted {
                    tracing::debug!(
                        vault = %cfg.global_vault,
                        profile_id = cfg.profile_id,
                        market = %market,
                        target_bps,
                        current_bps,
                        target_exposure_atoms,
                        on_chain_exposure_atoms = seat.max_exposure_atoms,
                        "within deltas — skipping"
                    );
                    return Ok(());
                }

                let mut ixs: Vec<Instruction> = Vec::with_capacity(2);
                if exposure_drifted {
                    ixs.push(set_seat_max_exposure_for_risk_profile_instruction(
                        &debt_mint,
                        market,
                        &fee_payer_pk,
                        &curator_pk,
                        cfg.profile_id,
                        clamp_target_above_deployed(target_exposure_atoms, seat.deployed_atoms),
                    ));
                }
                // Always replace the order when exposure changed (so
                // the resting ask's principal_atoms re-reads the new
                // cap) or when the rate drifted. Both reasons collapse
                // into the same cancel+replace update.
                ixs.push(update_order_for_risk_profile_instruction(
                    &debt_mint,
                    market,
                    &fee_payer_pk,
                    &curator_pk,
                    cfg.profile_id,
                    target_bps,
                    cfg.target_term_seconds,
                    0, // flags
                ));

                let label = match (exposure_drifted, rate_drifted) {
                    (true, true) => "resync_exposure_and_update_order",
                    (true, false) => "resync_exposure_and_update_order",
                    (false, true) => "update_order_for_risk_profile",
                    (false, false) => unreachable!(),
                };
                let sig = ctx
                    .rpc
                    .send_signed_labeled(label, ixs, &[&fee_payer, &curator])
                    .await?;
                tracing::info!(
                    vault = %cfg.global_vault,
                    profile_id = cfg.profile_id,
                    market = %market,
                    old_rate_bps = current_bps,
                    new_rate_bps = target_bps,
                    on_chain_exposure_atoms = seat.max_exposure_atoms,
                    target_exposure_atoms,
                    deployed_atoms = seat.deployed_atoms,
                    exposure_drifted,
                    rate_drifted,
                    sig = %sig,
                    "curator order reconciled"
                );
                self.last_update.lock().unwrap().insert(key, Instant::now());
            }
        }
        Ok(())
    }

    async fn dynamic_target(
        &self,
        ctx: &HandlerContext,
        debt_mint: &Pubkey,
        alpha_bps: u16,
    ) -> Result<u16> {
        let bank = ctx
            .cfg
            .banks
            .get(debt_mint)
            .ok_or_else(|| anyhow!("no BANKS config for {debt_mint}"))?;
        let raw = ctx
            .rpc
            .get_account_data(&bank.bank)
            .await?
            .ok_or_else(|| anyhow!("bank {} not found on-chain", bank.bank))?;
        let view = BankView::try_from_account_data(&raw)?;
        let snapshot = compute_rates(&view);
        tracing::trace!(
            mint = %debt_mint,
            util_bps = snapshot.utilization_bps(),
            borrow_bps = snapshot.borrow_apr_bps(),
            supply_bps = snapshot.supply_apr_bps(),
            "marginfi rate snapshot"
        );
        let target = target_rate_bps(&snapshot, alpha_bps);
        // An empty / brand-new marginfi bank produces (supply=0,
        // borrow=0), which the curve correctly maps to a target of 0.
        // Quoting at 0bps would mean lending for free — clearly not
        // what the operator wants. Treat zero as a degenerate signal
        // and let the caller fall back to `target_rate_fallback_bps`.
        if target == 0 {
            return Err(anyhow!(
                "marginfi rate snapshot is degenerate (target=0bps; supply={}, borrow={})",
                snapshot.supply_apr_bps(),
                snapshot.borrow_apr_bps()
            ));
        }
        Ok(target)
    }
}

/// Blend the profile's policy fields into a [0, 10_000] bps score.
/// Higher = more aggressive = larger share of the deposit pool. A
/// profile maxed out on every dimension scores 10_000 (full pool); a
/// profile at zero on every dimension scores 0 (no exposure).
fn risk_score_bps(profile: &RiskProfileView) -> u32 {
    let ltv = profile.max_ltv_bps as u32;
    // Cap term contribution at 1 year — beyond that, longer terms
    // don't keep adding "more aggressive" linearly to the score.
    let term = (((profile.max_term_seconds as u64) * 10_000)
        .saturating_div(TERM_NORMALIZATION_SECS) as u32)
        .min(10_000);
    let markets = ((profile.allowed_market_max as u32) * 10_000 / MARKETS_NORMALIZATION).min(10_000);

    let weighted =
        RISK_WEIGHT_LTV * ltv + RISK_WEIGHT_TERM * term + RISK_WEIGHT_MARKETS * markets;
    (weighted / RISK_WEIGHT_SUM).min(10_000)
}

/// `pool × risk_score / 10_000`, where the pool is the profile's live
/// deposits or the configured baseline when deposits are zero. The
/// program rejects `max_exposure_atoms == 0`, so floor at 1.
fn compute_target_max_exposure_atoms(
    profile: &RiskProfileView,
    cfg: &CuratorSignerConfig,
) -> u64 {
    let pool = if profile.total_principal_atoms > 0 {
        profile.total_principal_atoms
    } else {
        cfg.exposure_baseline_atoms
    };
    let score = risk_score_bps(profile) as u128;
    let raw = (pool as u128 * score / 10_000) as u64;
    raw.max(1)
}

/// On-chain `SetSeatMaxExposureForRiskProfile` rejects a new cap below
/// the seat's running `deployed_atoms`. When the computed target dips
/// below the in-flight tally (e.g. a depositor just withdrew), the best
/// we can do is hold the cap at the deployed floor and let repayments
/// drain it down naturally.
fn clamp_target_above_deployed(target: u64, deployed: u64) -> u64 {
    target.max(deployed.max(1))
}

/// True when the on-chain cap differs from the target by more than
/// `min_delta_bps`, measured against the larger of the two. Symmetric
/// so we don't churn on tiny up- vs. down-moves. The deployed floor
/// is folded in so we never schedule a no-op `set_seat_max_exposure`
/// (the program would reject and we'd burn a tx fee).
fn should_resync_exposure(on_chain: u64, target: u64, deployed: u64, min_delta_bps: u16) -> bool {
    let effective_target = clamp_target_above_deployed(target, deployed);
    if effective_target == on_chain {
        return false;
    }
    let diff = effective_target.abs_diff(on_chain) as u128;
    let denom = effective_target.max(on_chain) as u128;
    if denom == 0 {
        return false;
    }
    (diff * 10_000 / denom) as u32 >= min_delta_bps as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with(max_ltv_bps: u16, max_term_seconds: u32, allowed_market_max: u8) -> RiskProfileView {
        RiskProfileView {
            profile_id: 1,
            curator: Pubkey::default(),
            max_ltv_bps,
            max_term_seconds,
            allowed_market_count: 0,
            allowed_market_max,
            deployed_principal_atoms: 0,
            total_principal_atoms: 0,
            encumbered_in_orders_atoms: 0,
            active_markets: vec![],
        }
    }

    fn cfg_with_baseline(atoms: u64) -> CuratorSignerConfig {
        CuratorSignerConfig {
            global_vault: Pubkey::default(),
            profile_id: 1,
            keypair: crate::config::KeypairSource::Base58("x".into()),
            rate_target: RateTarget::Static { rate_bps: 0 },
            target_term_seconds: 0,
            managed_markets: vec![],
            exposure_baseline_atoms: atoms,
        }
    }

    #[test]
    fn risk_score_zero_profile_is_zero() {
        let p = profile_with(0, 0, 0);
        assert_eq!(risk_score_bps(&p), 0);
    }

    #[test]
    fn risk_score_max_profile_is_ten_thousand() {
        let p = profile_with(10_000, TERM_NORMALIZATION_SECS as u32, 8);
        assert_eq!(risk_score_bps(&p), 10_000);
    }

    #[test]
    fn risk_score_blend_matches_weights() {
        // 50% LTV (5000) × 0.5 + 25% term (10000) × 0.25 + 25% markets (10000) × 0.25
        // = 2500 + 2500 + 2500 = 7500
        let p = profile_with(5000, TERM_NORMALIZATION_SECS as u32, 8);
        assert_eq!(risk_score_bps(&p), 7500);
    }

    #[test]
    fn target_uses_baseline_when_no_deposits() {
        // Score = 5000, baseline = 500 USDC → target = 250 USDC.
        let p = profile_with(5000, TERM_NORMALIZATION_SECS as u32 / 2, 4);
        let cfg = cfg_with_baseline(500_000_000);
        // ltv 5000*50 + term 5000*25 + markets 5000*25 = 500_000 / 100 = 5000
        assert_eq!(risk_score_bps(&p), 5000);
        let t = compute_target_max_exposure_atoms(&p, &cfg);
        assert_eq!(t, 250_000_000);
    }

    #[test]
    fn target_scales_with_deposits() {
        let mut p = profile_with(8000, TERM_NORMALIZATION_SECS as u32, 8);
        p.total_principal_atoms = 10_000_000_000; // 10k USDC
        let cfg = cfg_with_baseline(500_000_000);
        // score = 0.5*8000 + 0.25*10_000 + 0.25*10_000 = 4000 + 2500 + 2500 = 9000
        assert_eq!(risk_score_bps(&p), 9000);
        let t = compute_target_max_exposure_atoms(&p, &cfg);
        assert_eq!(t, 9_000_000_000);
    }

    #[test]
    fn target_never_zero() {
        let p = profile_with(0, 0, 0);
        let cfg = cfg_with_baseline(500_000_000);
        // Score 0 × baseline = 0, but the program rejects 0 — we clamp to 1.
        let t = compute_target_max_exposure_atoms(&p, &cfg);
        assert_eq!(t, 1);
    }

    #[test]
    fn clamp_holds_floor_above_deployed() {
        assert_eq!(clamp_target_above_deployed(100, 500), 500);
        assert_eq!(clamp_target_above_deployed(1000, 500), 1000);
        assert_eq!(clamp_target_above_deployed(0, 0), 1);
    }

    #[test]
    fn resync_skips_within_threshold() {
        // 5% threshold, drift of 2% → skip
        assert!(!should_resync_exposure(1_000_000, 1_020_000, 0, 500));
    }

    #[test]
    fn resync_triggers_beyond_threshold() {
        // 5% threshold, drift of 10% → trigger
        assert!(should_resync_exposure(1_000_000, 1_100_000, 0, 500));
    }

    #[test]
    fn resync_skips_when_clamped_to_same_value() {
        // Target below deployed → clamp to deployed (which matches on-chain) → skip
        assert!(!should_resync_exposure(500, 100, 500, 500));
    }
}
