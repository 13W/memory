//! Controllable logical clocks for deterministic tests.
//!
//! Production time sources are injected through the [`Clock`] trait so tests can
//! substitute a clock they fully control. Values are logical nanoseconds
//! (`u64`); nothing here reads the system clock, so a test that fixes the clock
//! gets identical timestamps on every run.

use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonic source of logical timestamps in nanoseconds.
pub trait Clock {
    /// The current logical time, in nanoseconds.
    fn now_nanos(&self) -> u64;
}

/// A clock frozen at a single instant.
///
/// ```
/// use local_rag_test_support::{Clock, FixedClock};
/// let clock = FixedClock::new(1_000);
/// assert_eq!(clock.now_nanos(), 1_000);
/// assert_eq!(clock.now_nanos(), 1_000);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(u64);

impl FixedClock {
    /// Create a clock that always reports `nanos`.
    pub fn new(nanos: u64) -> Self {
        Self(nanos)
    }
}

impl Clock for FixedClock {
    fn now_nanos(&self) -> u64 {
        self.0
    }
}

/// A clock the test advances by hand.
///
/// Reading the clock never changes it — only [`ManualClock::advance`] and
/// [`ManualClock::set`] do. Two clocks driven through the same sequence of calls
/// report the same values, which is the property the reproducibility test
/// relies on.
///
/// ```
/// use local_rag_test_support::{Clock, ManualClock};
/// let clock = ManualClock::new(0);
/// assert_eq!(clock.now_nanos(), 0);
/// clock.advance(5);
/// clock.advance(5);
/// assert_eq!(clock.now_nanos(), 10);
/// clock.set(42);
/// assert_eq!(clock.now_nanos(), 42);
/// ```
#[derive(Debug)]
pub struct ManualClock {
    nanos: AtomicU64,
}

impl ManualClock {
    /// Create a manual clock starting at `nanos`.
    pub fn new(nanos: u64) -> Self {
        Self {
            nanos: AtomicU64::new(nanos),
        }
    }

    /// Advance the clock by `delta` nanoseconds and return the new value.
    pub fn advance(&self, delta: u64) -> u64 {
        self.nanos.fetch_add(delta, Ordering::Relaxed) + delta
    }

    /// Set the clock to an absolute `nanos` value.
    pub fn set(&self, nanos: u64) {
        self.nanos.store(nanos, Ordering::Relaxed);
    }
}

impl Clock for ManualClock {
    fn now_nanos(&self) -> u64 {
        self.nanos.load(Ordering::Relaxed)
    }
}
