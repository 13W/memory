//! Production wall-clock seam.
//!
//! The migration runner ([`crate::migrate`]) records `applied_at` as Unix
//! milliseconds (spec 03 §1.1). Time enters the runner as a plain `now_ms: i64`
//! parameter so tests can pass a fixed literal for byte-deterministic rows; this
//! helper is the one production source of that value. Kept separate from
//! `test-support`'s logical `Clock` (which is logical nanoseconds, for tests
//! only) so production code takes no dev-only dependency.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current wall-clock time as Unix milliseconds (spec 03 §1.1).
///
/// A pre-1970 clock (only possible if the system time is set absurdly) clamps to
/// `0` rather than panicking; migration timestamps are advisory bookkeeping, not
/// load-bearing for any constraint.
pub(crate) fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
