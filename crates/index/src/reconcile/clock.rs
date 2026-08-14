//! The reconcile loop's wall-clock seam (D-062).
//!
//! [`reconcile_once`](super::reconcile_once) and
//! [`build_generation`](super::build_generation) take `now_ms: i64` and write it
//! straight into `_at` columns, which spec 03 (`03-data-model.md:10`) defines as
//! **Unix milliseconds, UTC**. The one-shot CLI callers already pass a wall clock;
//! [`WorktreeReconciler`](super::WorktreeReconciler) — the long-lived loop behind
//! `local-rag watch` and the daemon's background indexing — used to pass its
//! debouncer's *monotonic* millisecond instead, so every row it wrote carried
//! milliseconds-since-loop-start (D-062: 3345 of 3350 `generation` rows on the
//! reporter's live store).
//!
//! The two clocks are now separate by construction: the monotonic origin stays
//! with the [`Debouncer`](super::Debouncer)'s scheduling arithmetic, and this
//! trait supplies the wall time for anything durable. It mirrors the id seam next
//! to it ([`UuidSource`](local_rag_core::identity::UuidSource) /
//! `SystemUuidV7`): production wires [`SystemWallClock`], tests wire a fixed
//! source so rows stay byte-deterministic.

/// Source of Unix-millisecond timestamps for durable `_at` columns (spec 03).
pub trait WallClock: Send + Sync {
    /// The current wall-clock time as Unix milliseconds, UTC.
    fn now_ms(&self) -> i64;
}

/// OS-backed [`WallClock`]: `SystemTime` since the Unix epoch.
///
/// A pre-1970 clock (only reachable if the system time is set absurdly) clamps to
/// `0` rather than panicking — the same choice `local_rag_store`'s own migration
/// clock makes for the same reason: these timestamps are bookkeeping, never
/// load-bearing for a constraint.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now_ms(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A [`WallClock`] frozen at one instant — the test seam.
///
/// Kept in production code (not behind `#[cfg(test)]`) because the integration
/// tests in `tests/` are separate crates and cannot reach a test-only item.
#[derive(Debug, Clone, Copy)]
pub struct FixedWallClock(pub i64);

impl WallClock for FixedWallClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production clock must land in Unix-millisecond territory — the exact
    /// property D-062 restored. `1_700_000_000_000` is 2023-11-14; a monotonic
    /// since-loop-start value (the bug) is always far below it, and the assert
    /// never races the wall clock because time only moves forward.
    #[test]
    fn system_wall_clock_returns_unix_milliseconds() {
        assert!(SystemWallClock.now_ms() > 1_700_000_000_000);
    }

    #[test]
    fn fixed_wall_clock_returns_its_literal() {
        assert_eq!(FixedWallClock(42).now_ms(), 42);
    }
}
