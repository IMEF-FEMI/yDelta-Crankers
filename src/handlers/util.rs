//! Small helpers shared between handlers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock unix seconds. Used for client-side maturity gating; the
/// on-chain ix uses its own `Clock::get()` so a few seconds of skew is
/// fine.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
