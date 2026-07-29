//! `PRAGMA wal_checkpoint` mode/result shared by [`crate::state::StateWriter`]
//! and [`crate::cache::CacheWriter`] (spec 02 §4.3, 03 §3) — T15-01.
//!
//! A plain value type, not a synchronization primitive, so sharing it across
//! the state/cache boundary does not weaken 03 §1.4's "no writable cross-DB
//! transaction" rule — each writer still runs its own `PRAGMA` against its own
//! connection, on its own queue.

/// A WAL checkpoint mode (spec 03 §3: "WAL checkpoint: `PASSIVE`
/// opportunistically; `TRUNCATE` when WAL > 64 MiB and no readers").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointMode {
    /// Checkpoint whatever can be done without blocking any writer/reader —
    /// the opportunistic, cheap-to-call-often mode.
    Passive,
    /// Checkpoint and attempt to truncate the `-wal` file to zero bytes.
    /// Never errors for failing to fully truncate (e.g. a reader still holds
    /// an older snapshot mapped into the WAL) — it simply checkpoints what it
    /// can and reports a smaller `checkpointed_frames` via [`CheckpointStats`].
    Truncate,
}

impl CheckpointMode {
    /// The literal `PRAGMA` text for this mode.
    pub(crate) fn pragma(self) -> &'static str {
        match self {
            CheckpointMode::Passive => "PRAGMA wal_checkpoint(PASSIVE)",
            CheckpointMode::Truncate => "PRAGMA wal_checkpoint(TRUNCATE)",
        }
    }
}

/// The reply of one `PRAGMA wal_checkpoint` call — SQLite's own three-column
/// shape (<https://www.sqlite.org/pragma.html#pragma_wal_checkpoint>).
///
/// Confirmed by direct reproduction against this crate's pinned (bundled)
/// SQLite: for [`CheckpointMode::Truncate`] specifically, `log_frames`/
/// `checkpointed_frames` both read `0` when it is the *first* checkpoint ever
/// run on a connection, even though the checkpoint and truncation both did
/// happen (the `-wal` file shrinks). [`CheckpointMode::Passive`] does not have
/// this quirk — its counters are accurate from the first call. Treat
/// `Truncate`'s counters as a best-effort diagnostic, never as the signal that
/// the checkpoint ran; the `-wal` file size is the reliable signal for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointStats {
    /// `true` if the checkpoint could not run to completion because another
    /// connection held a conflicting lock.
    pub busy: bool,
    /// The number of frames in the `-wal` file at the time of the call.
    pub log_frames: i64,
    /// The number of frames actually transferred into the database file.
    pub checkpointed_frames: i64,
}
