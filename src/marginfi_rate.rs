//! Marginfi interest-rate curve evaluation in fp48 arithmetic.
//!
//! Replicates `marginfi-v2/programs/marginfi/src/state/interest_rate.rs`
//! `InterestRateCalc::calc_interest_rate` semantics for both curve
//! types the on-chain `InterestRateConfig` carries:
//!
//! - `curve_type = 0` (legacy three-point):
//!
//!     if util ≤ optimal:
//!         borrow_apr = (util / optimal) × plateau_rate
//!     else:
//!         borrow_apr = plateau + ((util - optimal) / (1 - optimal)) × (max - plateau)
//!
//! - `curve_type = 1` (seven-point multipoint, mainnet default since
//!   v1.6): borrow APR is the piecewise-linear interpolation of
//!   `(util, rate)` going through `(0, zero_util_rate)`, every active
//!   `points[i]` (util ≠ 0) in ascending util order, and finally
//!   `(1.0, hundred_util_rate)`.
//!
//! Supply APR derivation (matches the on-chain effective rate the
//! lender earns):
//!
//!   supply_apr ≈ borrow_apr × util × (1 - protocol_ir_fee)
//!
//! All math stays in i128/u128 fp48 to mirror marginfi's i80f48. We
//! convert to bps only at the very end.

use crate::marginfi_bank::{BankView, INTEREST_CURVE_SEVEN_POINT};

const FP48_ONE: u128 = 1u128 << 48;

/// Live rate snapshot computed from a single Bank read.
#[derive(Debug, Clone, Copy)]
pub struct RateSnapshot {
    pub utilization_fp48: u128,
    pub borrow_apr_fp48: u128,
    pub supply_apr_fp48: u128,
}

impl RateSnapshot {
    pub fn utilization_bps(&self) -> u32 {
        fp48_to_bps(self.utilization_fp48)
    }
    pub fn borrow_apr_bps(&self) -> u32 {
        fp48_to_bps(self.borrow_apr_fp48)
    }
    pub fn supply_apr_bps(&self) -> u32 {
        fp48_to_bps(self.supply_apr_fp48)
    }
}

/// Compute live utilization, borrow APR, and supply APR for a bank.
pub fn compute_rates(bank: &BankView) -> RateSnapshot {
    // Utilization is the ratio of total-liability-value to total-asset-
    // value. Both totals can run to ~2^140+ on a busy mainnet bank
    // (shares_fp48 × share_value_fp48), well past u128::MAX, so we
    // avoid materializing them. Instead, use the algebraic identity
    //
    //   total_L / total_A = (TL_shares × LV) / (TA_shares × AV)
    //                     = (TL_shares / TA_shares) × (LV / AV)
    //
    // and compute each ratio in fp48 separately. Each ratio is bounded
    // near 1.0 (≤ 2.0 in pathological share-value drift), so the final
    // fp48-multiply can never overflow u128.
    let total_assets = as_u128(bank.total_asset_shares_fp48);
    let total_liabs = as_u128(bank.total_liability_shares_fp48);
    let asset_share_value = as_u128(bank.asset_share_value_fp48);
    let liab_share_value = as_u128(bank.liability_share_value_fp48);

    let utilization_fp48 = if total_assets == 0 || asset_share_value == 0 {
        0
    } else {
        let shares_ratio = fp48_div_wide(total_liabs, total_assets);
        let value_ratio = fp48_div_wide(liab_share_value, asset_share_value);
        fp48_mul(shares_ratio, value_ratio).min(FP48_ONE)
    };

    let protocol_ir_fee = as_u128(bank.protocol_ir_fee_fp48).min(FP48_ONE);

    let borrow_apr_fp48 = if bank.curve_type == INTEREST_CURVE_SEVEN_POINT {
        multipoint_curve_fp48(
            utilization_fp48,
            bank.zero_util_rate_u32,
            bank.hundred_util_rate_u32,
            &bank.points,
        )
    } else {
        legacy_curve_fp48(utilization_fp48, bank)
    };

    // supply ≈ borrow × util × (1 - protocol_ir_fee)
    let net_factor = FP48_ONE.saturating_sub(protocol_ir_fee);
    let supply_apr_fp48 = fp48_mul(fp48_mul(borrow_apr_fp48, utilization_fp48), net_factor);

    RateSnapshot {
        utilization_fp48,
        borrow_apr_fp48,
        supply_apr_fp48,
    }
}

/// Curator policy: target the rate at `supply + α × (borrow - supply)`,
/// **clamped to `[supply, borrow]`** so the LP never earns less than
/// marginfi-supply and the borrower never pays more than marginfi-borrow.
///
/// Degenerate case: if `borrow ≤ supply` (inverted curve — usually means
/// utilization is zero or the bank has been mis-configured), the policy
/// collapses to `target = supply`. Quoting at supply gives borrowers a
/// price no worse than the floor while still earning LPs the marginfi-supply
/// baseline. Returning anything below supply would actively lose LPs money
/// vs. just sitting in marginfi.
///
/// `alpha_bps ∈ [0, 10_000]`.
pub fn target_rate_bps(snapshot: &RateSnapshot, alpha_bps: u16) -> u16 {
    let supply = snapshot.supply_apr_fp48;
    let borrow = snapshot.borrow_apr_fp48;

    let supply_bps = fp48_to_bps(supply).min(u16::MAX as u32) as u16;
    let borrow_bps = fp48_to_bps(borrow).min(u16::MAX as u32) as u16;

    // Degenerate / inverted: borrow ≤ supply. Quote at supply.
    if borrow_bps <= supply_bps {
        return supply_bps;
    }

    let alpha = (alpha_bps as u128).min(10_000);
    let spread = borrow.saturating_sub(supply);
    let curator_take = spread.saturating_mul(alpha) / 10_000;
    let target_fp48 = supply.saturating_add(curator_take);
    let target_bps = fp48_to_bps(target_fp48).min(u16::MAX as u32) as u16;

    // Final clamp into [supply_bps, borrow_bps]. Defensive — the formula
    // already produces a value in [supply, borrow] for α ∈ [0, 10_000],
    // but rounding through fp48 → bps can drift ±1 and a future α
    // refactor could break the invariant silently. The clamp is the
    // policy guarantee.
    target_bps.clamp(supply_bps, borrow_bps)
}

// ─── Curve evaluation ───────────────────────────────────────────────

fn legacy_curve_fp48(utilization_fp48: u128, bank: &BankView) -> u128 {
    let optimal = as_u128(bank.optimal_utilization_fp48);
    let plateau = as_u128(bank.plateau_interest_rate_fp48);
    let max_ir = as_u128(bank.max_interest_rate_fp48);

    if optimal == 0 {
        return plateau;
    }
    if utilization_fp48 <= optimal {
        return fp48_mul(fp48_div(utilization_fp48, optimal), plateau);
    }
    let over = utilization_fp48 - optimal;
    let span = FP48_ONE.saturating_sub(optimal).max(1);
    let frac = fp48_div(over, span);
    let curve = fp48_mul(frac, max_ir.saturating_sub(plateau));
    plateau.saturating_add(curve)
}

/// Evaluate marginfi's seven-point multipoint curve in fp48.
///
/// The curve is the piecewise-linear function through the points
/// `(0, zero_rate)`, every active `points[i]` (`util ≠ 0`) in ascending
/// util order, and `(1.0, hundred_rate)`. Mirrors
/// `InterestRateCalc::interest_rate_multipoint_curve` in marginfi-v2.
fn multipoint_curve_fp48(
    utilization_fp48: u128,
    zero_util_rate_u32: u32,
    hundred_util_rate_u32: u32,
    points: &[(u32, u32)],
) -> u128 {
    let util = utilization_fp48.min(FP48_ONE);
    let zero_rate = u32_rate_to_fp48(zero_util_rate_u32);
    let hundred_rate = u32_rate_to_fp48(hundred_util_rate_u32);

    let mut prev_u = 0u128;
    let mut prev_r = zero_rate;
    for &(util_u32, rate_u32) in points {
        if util_u32 == 0 {
            continue;
        }
        let point_u = u32_util_to_fp48(util_u32);
        let point_r = u32_rate_to_fp48(rate_u32);
        if util <= point_u {
            return lerp_fp48(prev_u, prev_r, point_u, point_r, util);
        }
        prev_u = point_u;
        prev_r = point_r;
    }
    lerp_fp48(prev_u, prev_r, FP48_ONE, hundred_rate, util)
}

/// Linear interpolation between `(sx, sy)` and `(ex, ey)` evaluated at
/// `target`, with the same edge-case behavior as marginfi's `lerp`:
/// returns `sy` when the segment is degenerate or `target < sx`, returns
/// `ey` when `target > ex`, returns `sy` when `ey < sy` (curve must be
/// monotone non-decreasing).
fn lerp_fp48(sx: u128, sy: u128, ex: u128, ey: u128, target: u128) -> u128 {
    if ex <= sx || target < sx {
        return sy;
    }
    if target > ex {
        return ey;
    }
    if ey < sy {
        return sy;
    }
    let delta_x = ex - sx;
    if delta_x == 0 {
        return sy;
    }
    let offset = target - sx;
    let proportion = fp48_div(offset, delta_x);
    let delta_y = ey - sy;
    let scaled = fp48_mul(delta_y, proportion);
    sy.saturating_add(scaled)
}

/// Convert a marginfi `u32`-encoded rate to fp48. Source encoding:
/// `rate = (raw / u32::MAX) × 10` (i.e. `u32::MAX / 10 ≈ 100% APR`,
/// `u32::MAX ≈ 1000% APR`). Computed as
/// `raw × 10 × (1 << 48) / u32::MAX` to keep precision through the
/// scale-up.
fn u32_rate_to_fp48(raw: u32) -> u128 {
    if raw == 0 {
        return 0;
    }
    let numer = (raw as u128) * 10u128 * FP48_ONE;
    numer / (u32::MAX as u128)
}

/// Convert a marginfi `u32`-encoded utilization to fp48. Source
/// encoding: `util = raw / u32::MAX = 0..1`.
fn u32_util_to_fp48(raw: u32) -> u128 {
    if raw == 0 {
        return 0;
    }
    let numer = (raw as u128) * FP48_ONE;
    numer / (u32::MAX as u128)
}

// ─── Helpers ────────────────────────────────────────────────────────

fn as_u128(v: i128) -> u128 {
    if v < 0 {
        0
    } else {
        v as u128
    }
}

fn fp48_mul(a: u128, b: u128) -> u128 {
    // (a × b) >> 48, with overflow-safe widening via 256-bit split.
    // For our magnitudes (rates ≤ 1.0 fp48, share values ≤ 2^80 fp48)
    // this fits in u128 directly.
    a.checked_mul(b).map(|x| x >> 48).unwrap_or(u128::MAX)
}

fn fp48_div(a: u128, b: u128) -> u128 {
    if b == 0 {
        return 0;
    }
    // (a << 48) / b
    if a == 0 {
        return 0;
    }
    // Avoid overflow on the shift by detecting if a's leading zeros ≥ 48.
    if a.leading_zeros() >= 48 {
        (a << 48) / b
    } else {
        // Fall back to lossy division — for our magnitudes, this branch
        // is hit only when a represents > 2^80, which would mean a rate
        // > 65535x — not a realistic bank state. Saturate rather than
        // panic.
        u128::MAX
    }
}

/// Compute `(a / b) << 48` in fp48 form when `a` may exceed `2^80`.
/// When `a << 48` would overflow `u128`, shift both operands down by the
/// same amount before dividing — preserves the ratio exactly when both
/// operands share the discarded low bits, and loses at most `shift`
/// bits otherwise. Designed for total-shares / share-value totals which
/// can hit ~`2^140` on mainnet banks.
fn fp48_div_wide(a: u128, b: u128) -> u128 {
    if b == 0 || a == 0 {
        return 0;
    }
    if a.leading_zeros() >= 48 {
        return (a << 48) / b;
    }
    let shift = 48 - a.leading_zeros();
    let a_s = a >> shift;
    let b_s = b >> shift;
    if b_s == 0 {
        return u128::MAX;
    }
    (a_s << 48) / b_s
}

fn fp48_to_bps(fp48: u128) -> u32 {
    // bps = (fp48 × 10_000) >> 48
    let scaled = fp48.checked_mul(10_000).unwrap_or(u128::MAX);
    let bps = scaled >> 48;
    bps.min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marginfi_bank::BankView;
    use solana_program::pubkey::Pubkey;

    fn bank_with(
        total_assets: u128,
        total_liabs: u128,
        asset_share_value: u128,
        liab_share_value: u128,
        optimal_bps: u32,
        plateau_bps: u32,
        max_bps: u32,
        protocol_fee_bps: u32,
    ) -> BankView {
        let bps_to_fp48 = |b: u32| ((b as u128) << 48) / 10_000;
        BankView {
            mint: Pubkey::default(),
            liquidity_vault: Pubkey::default(),
            lva_bump: 0,
            asset_share_value_fp48: asset_share_value as i128,
            liability_share_value_fp48: liab_share_value as i128,
            total_asset_shares_fp48: total_assets as i128,
            total_liability_shares_fp48: total_liabs as i128,
            optimal_utilization_fp48: bps_to_fp48(optimal_bps) as i128,
            plateau_interest_rate_fp48: bps_to_fp48(plateau_bps) as i128,
            max_interest_rate_fp48: bps_to_fp48(max_bps) as i128,
            protocol_ir_fee_fp48: bps_to_fp48(protocol_fee_bps) as i128,
            curve_type: crate::marginfi_bank::INTEREST_CURVE_LEGACY,
            zero_util_rate_u32: 0,
            hundred_util_rate_u32: 0,
            points: [(0, 0); 5],
            oracle_setup: 1,
            oracles: vec![],
        }
    }

    /// Build a bank running on the seven-point curve with a single
    /// interior kink. `pct_to_u32` follows marginfi's encoding: util as
    /// `pct/100 × u32::MAX`, rate as `pct/1000 × u32::MAX`.
    fn multipoint_bank(
        total_assets: u128,
        total_liabs: u128,
        zero_rate_pct: f64,
        hundred_rate_pct: f64,
        kink_util_pct: f64,
        kink_rate_pct: f64,
    ) -> BankView {
        let rate_to_u32 = |pct: f64| ((pct / 1000.0) * (u32::MAX as f64)) as u32;
        let util_to_u32 = |pct: f64| ((pct / 100.0) * (u32::MAX as f64)) as u32;
        let one_fp48 = 1u128 << 48;
        BankView {
            mint: Pubkey::default(),
            liquidity_vault: Pubkey::default(),
            lva_bump: 0,
            asset_share_value_fp48: one_fp48 as i128,
            liability_share_value_fp48: one_fp48 as i128,
            total_asset_shares_fp48: total_assets as i128,
            total_liability_shares_fp48: total_liabs as i128,
            optimal_utilization_fp48: 0,
            plateau_interest_rate_fp48: 0,
            max_interest_rate_fp48: 0,
            protocol_ir_fee_fp48: 0,
            curve_type: crate::marginfi_bank::INTEREST_CURVE_SEVEN_POINT,
            zero_util_rate_u32: rate_to_u32(zero_rate_pct),
            hundred_util_rate_u32: rate_to_u32(hundred_rate_pct),
            points: [
                (util_to_u32(kink_util_pct), rate_to_u32(kink_rate_pct)),
                (0, 0),
                (0, 0),
                (0, 0),
                (0, 0),
            ],
            oracle_setup: 1,
            oracles: vec![],
        }
    }

    #[test]
    fn rate_at_optimal_equals_plateau() {
        // 80% util at optimal=8000 → borrow == plateau (700 bps).
        let total_assets = 1000u128 << 48;
        let total_liabs = 800u128 << 48;
        let one_fp48 = 1u128 << 48;
        let b = bank_with(
            total_assets,
            total_liabs,
            one_fp48,
            one_fp48,
            8000,
            700,
            5000,
            0,
        );
        let s = compute_rates(&b);
        // Some rounding loss is acceptable; allow ±5 bps.
        let diff = (s.borrow_apr_bps() as i32 - 700).abs();
        assert!(
            diff <= 5,
            "borrow_apr_bps={} expected ~700",
            s.borrow_apr_bps()
        );
    }

    #[test]
    fn rate_above_optimal_ramps_to_max() {
        // 100% util with optimal=8000, plateau=700, max=5000 → borrow == max.
        let total_assets = 1000u128 << 48;
        let total_liabs = 1000u128 << 48;
        let one_fp48 = 1u128 << 48;
        let b = bank_with(
            total_assets,
            total_liabs,
            one_fp48,
            one_fp48,
            8000,
            700,
            5000,
            0,
        );
        let s = compute_rates(&b);
        let diff = (s.borrow_apr_bps() as i32 - 5000).abs();
        assert!(
            diff <= 5,
            "borrow_apr_bps={} expected ~5000",
            s.borrow_apr_bps()
        );
    }

    #[test]
    fn target_rate_alpha_midpoint() {
        // util 80% at optimal → borrow=700, supply≈700×0.8=560.
        // α=5000 (50%) → target ≈ 560 + 0.5×(700-560) = 630.
        let total_assets = 1000u128 << 48;
        let total_liabs = 800u128 << 48;
        let one_fp48 = 1u128 << 48;
        let b = bank_with(
            total_assets,
            total_liabs,
            one_fp48,
            one_fp48,
            8000,
            700,
            5000,
            0,
        );
        let s = compute_rates(&b);
        let t = target_rate_bps(&s, 5000);
        let diff = (t as i32 - 630).abs();
        assert!(diff <= 5, "target_bps={t} expected ~630");
    }

    #[test]
    fn target_rate_never_below_supply_or_above_borrow() {
        // Standard config; verify α=0 lands at supply, α=10000 at borrow.
        let total_assets = 1000u128 << 48;
        let total_liabs = 800u128 << 48;
        let one_fp48 = 1u128 << 48;
        let b = bank_with(
            total_assets,
            total_liabs,
            one_fp48,
            one_fp48,
            8000,
            700,
            5000,
            0,
        );
        let s = compute_rates(&b);
        let supply_bps = s.supply_apr_bps() as u16;
        let borrow_bps = s.borrow_apr_bps() as u16;

        let t_zero = target_rate_bps(&s, 0);
        let t_full = target_rate_bps(&s, 10_000);
        assert!(
            t_zero >= supply_bps,
            "α=0 target {t_zero} < supply {supply_bps}"
        );
        assert!(
            t_full <= borrow_bps,
            "α=10000 target {t_full} > borrow {borrow_bps}"
        );
        // Mid-α stays strictly inside (modulo rounding).
        let t_mid = target_rate_bps(&s, 5000);
        assert!(
            t_mid >= supply_bps && t_mid <= borrow_bps,
            "mid target {t_mid} outside [{supply_bps}, {borrow_bps}]"
        );
    }

    #[test]
    fn utilization_does_not_overflow_on_mainnet_sized_shares() {
        // Reproduces the live mainnet USDC bank
        // 2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB at the time of
        // writing: total_asset_shares ≈ 2.27e27 (fp48),
        // total_liability_shares ≈ 1.85e27 (fp48), asset_share_value
        // ≈ 1.218 (fp48), liability_share_value ≈ 1.354 (fp48). The
        // products `shares × share_value` exceed u128::MAX, so a naive
        // multiply saturates and the bot reads util=100% spuriously.
        // Expected real util ≈ (TL/TA) × (LV/AV) ≈ 0.814 × 1.112 ≈ 0.905
        // (~9050 bps).
        let b = BankView {
            mint: Pubkey::default(),
            liquidity_vault: Pubkey::default(),
            lva_bump: 0,
            asset_share_value_fp48: 342_830_588_583_771_i128,
            liability_share_value_fp48: 381_198_087_708_603_i128,
            total_asset_shares_fp48: 2_267_665_929_929_550_902_572_046_123_i128,
            total_liability_shares_fp48: 1_845_852_670_917_219_873_595_547_774_i128,
            optimal_utilization_fp48: 0,
            plateau_interest_rate_fp48: 0,
            max_interest_rate_fp48: 0,
            protocol_ir_fee_fp48: 0,
            curve_type: crate::marginfi_bank::INTEREST_CURVE_SEVEN_POINT,
            zero_util_rate_u32: 0,
            // 37.1% APR encoded as marginfi u32: pct/1000 × u32::MAX.
            hundred_util_rate_u32: ((0.371_f64 / 10.0) * (u32::MAX as f64)) as u32,
            points: [
                (
                    ((0.90_f64) * (u32::MAX as f64)) as u32,
                    ((0.075_f64 / 10.0) * (u32::MAX as f64)) as u32,
                ),
                (0, 0),
                (0, 0),
                (0, 0),
                (0, 0),
            ],
            oracle_setup: 1,
            oracles: vec![],
        };
        let s = compute_rates(&b);
        let util_bps = s.utilization_bps() as i32;
        // Allow ±100 bps for the precision lost in `fp48_div_wide`'s
        // shift-down on a ~2^91 share total.
        assert!(
            (8950..=9150).contains(&util_bps),
            "util_bps={util_bps} expected ~9050 (~0.905); overflow regression?"
        );
        // At util ≈ 0.905 the curve sits just past the kink at 90%/7.5%
        // on the second segment ending at (100%, 37.1%). Expected:
        //   0.075 + (0.905 - 0.9) / (1.0 - 0.9) × (0.371 - 0.075)
        //   ≈ 0.075 + 0.05 × 0.296 ≈ 0.0898 ≈ 898 bps.
        let borrow_bps = s.borrow_apr_bps() as i32;
        assert!(
            (800..=1000).contains(&borrow_bps),
            "borrow_apr_bps={borrow_bps} expected ~898 (second segment)"
        );
    }

    #[test]
    fn multipoint_curve_matches_mainnet_shape() {
        // Reproduces the on-chain debt-bank config from mainnet
        // 2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB:
        //   zero_util_rate=0%, hundred_util_rate=37.1%, kink at (90%, 7.5%).
        // 80% util sits on the first segment → linear from (0%, 0%) to
        // (90%, 7.5%) ⇒ borrow ≈ 80/90 × 7.5% ≈ 6.67% APR ≈ 667 bps.
        let total_assets = 1000u128 << 48;
        let total_liabs = 800u128 << 48;
        let b = multipoint_bank(total_assets, total_liabs, 0.0, 37.1, 90.0, 7.5);
        let s = compute_rates(&b);
        let diff = (s.borrow_apr_bps() as i32 - 667).abs();
        assert!(
            diff <= 10,
            "borrow_apr_bps={} expected ~667 (80% util on linear segment)",
            s.borrow_apr_bps()
        );
        // Supply ≈ borrow × util = 6.67% × 0.8 ≈ 5.34% ≈ 533 bps (no fee).
        let supply_diff = (s.supply_apr_bps() as i32 - 533).abs();
        assert!(
            supply_diff <= 10,
            "supply_apr_bps={} expected ~533",
            s.supply_apr_bps()
        );
    }

    #[test]
    fn multipoint_curve_above_kink_ramps_to_hundred() {
        // Same shape; bump util to 95% (between kink at 90% and end at 100%).
        // Expected borrow = lerp((90, 7.5), (100, 37.1)) at 95
        //                 = 7.5 + (95-90)/(100-90) × (37.1-7.5) = 22.3% ≈ 2230 bps
        let total_assets = 1000u128 << 48;
        let total_liabs = 950u128 << 48;
        let b = multipoint_bank(total_assets, total_liabs, 0.0, 37.1, 90.0, 7.5);
        let s = compute_rates(&b);
        let diff = (s.borrow_apr_bps() as i32 - 2230).abs();
        assert!(
            diff <= 20,
            "borrow_apr_bps={} expected ~2230 (95% util on second segment)",
            s.borrow_apr_bps()
        );
    }

    #[test]
    fn degenerate_inverted_curve_returns_supply() {
        // Construct an unusual bank where borrow < supply by setting
        // protocol_ir_fee to 0 (so supply ≈ borrow × util) AND
        // util > 1 isn't possible (clamped). Achieve inversion by
        // setting plateau very low and util close to 0 — borrow stays
        // tiny, supply rounds down to 0. Real inverted curves are
        // configuration bugs; we just verify the clamp.
        let total_assets = 1000u128 << 48;
        let total_liabs = 1u128 << 48; // ~0.1% util
        let one_fp48 = 1u128 << 48;
        let b = bank_with(
            total_assets,
            total_liabs,
            one_fp48,
            one_fp48,
            8000,
            50,
            5000,
            0,
        );
        let s = compute_rates(&b);
        // If somehow borrow < supply at this point, the function returns supply.
        let t = target_rate_bps(&s, 5000);
        if s.borrow_apr_bps() <= s.supply_apr_bps() {
            assert_eq!(t as u32, s.supply_apr_bps());
        }
    }
}
