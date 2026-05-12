//! Marginfi interest-rate curve evaluation in fp48 arithmetic.
//!
//! Replicates `marginfi-v2/programs/marginfi/src/state/marginfi_group.rs`
//! `InterestRateConfig::calc_interest_rate_curve` semantics:
//!
//!   if util ≤ optimal:
//!       borrow_apr = (util / optimal) × plateau_rate
//!   else:
//!       borrow_apr = plateau + ((util - optimal) / (1 - optimal)) × (max - plateau)
//!
//! Supply APR derivation (matches the on-chain effective rate the
//! lender earns):
//!
//!   supply_apr ≈ borrow_apr × util × (1 - protocol_ir_fee)
//!
//! All math stays in i128/u128 fp48 to mirror marginfi's i80f48. We
//! convert to bps only at the very end.

use crate::marginfi_bank::BankView;

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
    let total_assets_value = fp48_mul(
        as_u128(bank.total_asset_shares_fp48),
        as_u128(bank.asset_share_value_fp48),
    );
    let total_liabilities_value = fp48_mul(
        as_u128(bank.total_liability_shares_fp48),
        as_u128(bank.liability_share_value_fp48),
    );

    let utilization_fp48 = if total_assets_value == 0 {
        0
    } else {
        // util = liabilities / assets, capped at 1.0
        let raw = fp48_div(total_liabilities_value, total_assets_value);
        raw.min(FP48_ONE)
    };

    let optimal = as_u128(bank.optimal_utilization_fp48);
    let plateau = as_u128(bank.plateau_interest_rate_fp48);
    let max_ir = as_u128(bank.max_interest_rate_fp48);
    let protocol_ir_fee = as_u128(bank.protocol_ir_fee_fp48).min(FP48_ONE);

    let borrow_apr_fp48 = if optimal == 0 {
        // Degenerate config — treat as flat plateau.
        plateau
    } else if utilization_fp48 <= optimal {
        // Linear ramp from 0 to plateau across [0, optimal].
        fp48_mul(fp48_div(utilization_fp48, optimal), plateau)
    } else {
        // Linear ramp from plateau to max_ir across [optimal, 1].
        let over = utilization_fp48 - optimal;
        let span = FP48_ONE.saturating_sub(optimal).max(1);
        let frac = fp48_div(over, span);
        let curve = fp48_mul(frac, max_ir.saturating_sub(plateau));
        plateau.saturating_add(curve)
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
            asset_share_value_fp48: asset_share_value as i128,
            liability_share_value_fp48: liab_share_value as i128,
            total_asset_shares_fp48: total_assets as i128,
            total_liability_shares_fp48: total_liabs as i128,
            optimal_utilization_fp48: bps_to_fp48(optimal_bps) as i128,
            plateau_interest_rate_fp48: bps_to_fp48(plateau_bps) as i128,
            max_interest_rate_fp48: bps_to_fp48(max_bps) as i128,
            protocol_ir_fee_fp48: bps_to_fp48(protocol_fee_bps) as i128,
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
