//! Shared test harness for the `local-rag` workspace.
//!
//! Every group in the implementation plan needs tests that are deterministic
//! and independent of the machine they run on: no network, no real wall-clock,
//! and — critically — no dependency on the user's home directory. This crate
//! supplies the primitives that make such tests possible:
//!
//! - [`home`] — an isolated, self-cleaning temporary `LOCAL_RAG_HOME`.
//! - [`clock`] — controllable logical clocks ([`FixedClock`], [`ManualClock`]).
//! - [`ids`] — reproducible identifier sources ([`SeqUuids`]).
//! - [`subprocess`] — run a child process, capture its output, and persist an
//!   artifact bundle when it exits abnormally.
//! - [`failpoint`] — named, `fail_point!`-style injection points used by the
//!   F1–F12 projection and S1–S8 spool crash matrices (authored later in
//!   T07-05 / T13-06).
//! - [`fixtures`] — locate and read the behavioral fixtures imported by T00-01.
//!
//! # Determinism contract
//!
//! Nothing here reads `$HOME` or any user configuration. Temporary state lives
//! under [`std::env::temp_dir`] and is removed on drop. Clocks and id sources
//! are logical, so a test that fixes their seed gets byte-identical results on
//! every run and platform.
//!
//! This crate is **dev-only**: it is a workspace member so `cargo test
//! --workspace` covers it, but it is excluded from `default-members` and is
//! never a distributed binary.

#![doc(test(attr(deny(warnings))))]

pub mod clock;
pub mod failpoint;
pub mod fixtures;
pub mod home;
pub mod ids;
pub mod subprocess;

pub use clock::{Clock, FixedClock, ManualClock};
pub use failpoint::{Action, FailpointError, Failpoints};
pub use home::TempHome;
pub use ids::{IdSource, SeqUuids};
pub use subprocess::{RunOutcome, run_capturing};

/// Process-unique monotonic counter shared by the temp-home and artifact-bundle
/// naming schemes.
///
/// Combined with [`std::process::id`], this yields directory names that are
/// unique across concurrent tests in one process *and* across processes,
/// without consulting the wall clock (which would reintroduce nondeterminism
/// and races).
pub(crate) fn next_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
