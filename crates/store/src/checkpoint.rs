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

/// spec 03 §3's threshold, verbatim: "`TRUNCATE` when WAL > 64 MiB and no
/// readers".
///
/// `[SPEC]` in that section, so it is a number this crate may state rather than
/// derive. The threshold's purpose is that a blocking truncate is not paid too
/// often — not that the WAL is kept below it at all times, which nothing can
/// promise while a reader holds a snapshot.
pub const WAL_TRUNCATE_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

/// The size of the `-wal` file sitting beside `db_path`, or `0` when there is
/// none (no WAL yet, or it has just been truncated away).
///
/// The file size, not [`CheckpointStats`], is what a caller should test: this
/// module's own note above records that `Truncate`'s counters read zero on a
/// connection's first checkpoint even when it truncated. An unreadable path is
/// `0` for the same reason a missing one is — the caller's question is "is
/// there enough WAL to be worth a blocking truncate", and "cannot tell" answers
/// it with "not now" rather than with an error nobody could act on.
pub fn wal_bytes(db_path: &std::path::Path) -> u64 {
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push("-wal");
    std::fs::metadata(db_path.with_file_name(name))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Whether spec 03 §3's `TRUNCATE` clause fires right now (D-086).
///
/// Both halves matter and the second is the subtle one. "No readers" is not
/// politeness towards concurrent queries — a reader holding a snapshot pins the
/// frames after it, so a truncate under one transfers what it may and leaves the
/// file at its high-water mark, having taken the blocking cost for nothing.
/// Measured on the owner's store: during one 2.8-minute indexing cycle the
/// `-wal` grew to 2.5 GB at roughly 0.9 GB/min, all of it pinned by the embedding
/// backfill's read connection, and only the cycle's own end-of-run truncate
/// returned it.
pub fn should_truncate_wal(wal_bytes: u64, threshold_bytes: u64, readers_open: bool) -> bool {
    wal_bytes > threshold_bytes && !readers_open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_truncate_clause_needs_both_halves() {
        let over = WAL_TRUNCATE_THRESHOLD_BYTES + 1;
        let under = WAL_TRUNCATE_THRESHOLD_BYTES;
        for (wal, readers, expected, why) in [
            (
                over,
                false,
                true,
                "over the threshold with no reader: truncate",
            ),
            (
                over,
                true,
                false,
                "a reader pins the frames; truncating buys nothing",
            ),
            (under, false, false, "at the threshold is not over it"),
            (under, true, false, "neither half holds"),
            (0, false, false, "no WAL at all"),
        ] {
            assert_eq!(
                should_truncate_wal(wal, WAL_TRUNCATE_THRESHOLD_BYTES, readers),
                expected,
                "{why}"
            );
        }
    }

    #[test]
    fn a_missing_wal_file_measures_zero_rather_than_erroring() {
        let dir = std::env::temp_dir().join("local-rag-wal-bytes-probe");
        assert_eq!(wal_bytes(&dir.join("no-such.sqlite")), 0);
    }
}
