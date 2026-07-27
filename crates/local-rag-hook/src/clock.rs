//! Production wall-clock seam.
//!
//! `captured_at`/`coarse_ts` (spec 07 §3/§4) enter the write path as a plain
//! `now_ms: i64` parameter so tests can pass a fixed literal for deterministic
//! identity/frame assertions; this helper is the one production source of that
//! value. Mirrors `local_rag_store::clock::system_now_ms` exactly — kept
//! separate from `test-support`'s logical `Clock` (dev-only) so production code
//! takes no dev-only dependency.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current wall-clock time as Unix milliseconds.
///
/// A pre-1970 clock (only possible if the system time is set absurdly) clamps
/// to `0` rather than panicking; `captured_at` is observational metadata, not
/// load-bearing for any constraint.
pub fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
