use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// SPL token account `amount` field (legacy + token-2022 share the same
/// 64..72 offset).
pub fn spl_token_amount(data: &[u8]) -> Option<u64> {
    data.get(64..72)
        .map(|s| u64::from_le_bytes(s.try_into().expect("8 bytes")))
}

/// Mirrors the P2Pool full-close over-stage the program requires
/// (`settle_matured_loan.rs`): marginfi accrues liability_share_value
/// on entry, so we have to over-stage and let the program refund the
/// remainder.
pub fn p2pool_full_repay_staged_atoms(repay_atoms: u64) -> u64 {
    repay_atoms
        .saturating_add(repay_atoms / 50)
        .saturating_add(64)
}

/// Program's per-partial floor: `max(1% of outstanding, 1000 atoms)`.
pub fn min_partial_repay_atoms(outstanding: u64) -> u64 {
    const FLOOR: u64 = 1_000;
    (outstanding / 100).max(FLOOR)
}
