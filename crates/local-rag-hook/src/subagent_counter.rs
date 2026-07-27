//! Durable, crash-safe per-session, per-agent `stop_occurrence` counter for
//! `SubagentStop` identity (spec 07 §4: `ss:<session>:<agent_id>:<stop_occurrence>`).
//!
//! Claude Code gives no occurrence counter for `SubagentStop`, and the hook is
//! a fresh, stateless process per invocation — so a durable, monotonic count
//! has to live on disk. **Two files, not one**: a `File::lock()` only
//! protects a specific inode, so the file the *next* caller locks must be the
//! same one the *previous* caller locked — the lock file is therefore never
//! replaced by a rename. The counter's on-disk *value*, in contrast, is safe
//! to atomically replace (write-new + fsync + rename) precisely because
//! access to it is always gated by holding the separate, stable lock first —
//! mirroring `crates/store/src/migrate/lock.rs`'s `MigrationLock`, which locks
//! `migration.lock`, a file distinct from the `state.sqlite` it protects. A
//! design that locked the *data* file itself and then renamed a replacement
//! over it would defeat the lock: the next caller's fresh `open()` would land
//! on the new, never-yet-locked inode and race the previous caller instead of
//! serializing behind it.
//!
//! # What this does and does not guarantee
//!
//! Claude Code never learns a hook invocation failed (hooks always exit 0,
//! `[FIXED]`), so it almost certainly never *deliberately* retries a hook call
//! — the "at-least-once delivery" language (spec 07 §1) describes the general
//! spool-crash story, not a literal retry loop for this specific event. This
//! counter correctly guarantees that **distinct** real stops always receive
//! distinct, monotonically increasing occurrence numbers, and that a crash
//! mid-segment-append (a torn frame, S1) never corrupts or skips the count —
//! the counter update and the segment append are two independent durable
//! operations. What it structurally **cannot** do is distinguish "Claude Code
//! double-fired the hook for one logical stop" from "two genuinely distinct
//! stops": Claude Code provides no correlating signal for that, so every
//! invocation gets a fresh number by design. This is an information-theoretic
//! limit, not something better engineering here closes.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use local_rag_core::paths::{PathError, ensure_dir, ensure_file_0600};

const LOCK_FILE_NAME: &str = ".subagent_stop_seq.lock";
const DATA_FILE_NAME: &str = ".subagent_stop_seq.json";
const TMP_FILE_NAME: &str = ".subagent_stop_seq.json.tmp";

/// A failure reading or updating the durable stop-occurrence counter.
#[derive(Debug)]
pub enum CounterError {
    /// A filesystem operation failed.
    Io(io::Error),
    /// The counter file exists but is not valid JSON.
    ///
    /// A hard error — skip *this* `SubagentStop` event (fail-open) — rather
    /// than silently resetting to an empty map: a silent reset risks
    /// reissuing a `stop_occurrence` value already used by a previous,
    /// already-imported envelope for the same agent, producing a false
    /// `dedup_key` collision against permanently stored history. Losing one
    /// observation is a smaller failure than that.
    Corrupt(serde_json::Error),
    /// A store-path/permission error from the shared primitives.
    Path(PathError),
}

impl std::fmt::Display for CounterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CounterError::Io(e) => write!(f, "stop-occurrence counter i/o error: {e}"),
            CounterError::Corrupt(e) => write!(f, "stop-occurrence counter file is corrupt: {e}"),
            CounterError::Path(e) => write!(f, "stop-occurrence counter path error: {e}"),
        }
    }
}

impl std::error::Error for CounterError {}

impl From<io::Error> for CounterError {
    fn from(e: io::Error) -> Self {
        CounterError::Io(e)
    }
}

impl From<PathError> for CounterError {
    fn from(e: PathError) -> Self {
        CounterError::Path(e)
    }
}

/// Issue the next `stop_occurrence` for `agent_id` within `session_dir`
/// (`spool/<session_id>/`), durably and atomically under an exclusive lock.
pub fn next_stop_occurrence(session_dir: &Path, agent_id: &str) -> Result<u64, CounterError> {
    ensure_dir(session_dir)?;

    let lock_path = session_dir.join(LOCK_FILE_NAME);
    ensure_file_0600(&lock_path)?;
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock_file.lock()?;

    let data_path = session_dir.join(DATA_FILE_NAME);
    let mut counts = read_counts(&data_path)?;

    let next = counts.get(agent_id).copied().unwrap_or(0) + 1;
    counts.insert(agent_id.to_string(), next);

    write_counts_atomically(session_dir, &data_path, &counts)?;

    let _ = lock_file.unlock();
    Ok(next)
}

/// Read the counter map, treating a missing or empty file as empty. A
/// present-but-unparsable file is [`CounterError::Corrupt`], never silently
/// reset.
fn read_counts(data_path: &Path) -> Result<HashMap<String, u64>, CounterError> {
    match fs::read(data_path) {
        Ok(bytes) if bytes.is_empty() => Ok(HashMap::new()),
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(CounterError::Corrupt),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(CounterError::Io(e)),
    }
}

/// Write `counts` via write-new-file + `fdatasync` + atomic `rename` over
/// `data_path` — never truncate-in-place, which is not crash-atomic (a kill
/// between truncate and write would leave the counter file empty, silently
/// resetting every agent's count in this session on the next invocation).
fn write_counts_atomically(
    session_dir: &Path,
    data_path: &Path,
    counts: &HashMap<String, u64>,
) -> Result<(), CounterError> {
    let tmp_path = session_dir.join(TMP_FILE_NAME);
    let bytes = serde_json::to_vec(counts).expect("a string-keyed u64 map always serializes");

    ensure_file_0600(&tmp_path)?;
    {
        let mut tmp_file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;
        tmp_file.write_all(&bytes)?;
        tmp_file.sync_data()?;
    }
    fs::rename(&tmp_path, data_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_test_support::TempHome;
    use std::path::PathBuf;

    /// A session directory under `spool/`, with `spool/` itself pre-created —
    /// in production `StoreLayout::ensure()` creates it at store-init time
    /// (a precondition the whole daemon already relies on), so `ensure_dir`
    /// only ever needs to create the session-specific leaf. `ensure_dir`
    /// itself is not recursive (mirrors `StoreLayout::ensure()`'s own
    /// `create_dir_all(parent)` + strict `ensure_dir(root)` split), so tests
    /// must set up that same precondition explicitly.
    fn session_dir(home: &TempHome, session_id: &str) -> PathBuf {
        let spool = home.join("spool");
        std::fs::create_dir_all(&spool).expect("spool dir");
        spool.join(session_id)
    }

    #[test]
    fn sequential_increments_per_agent() {
        let home = TempHome::new().expect("temp home");
        let session_dir = session_dir(&home, "sess-1");
        assert_eq!(next_stop_occurrence(&session_dir, "agent-a").unwrap(), 1);
        assert_eq!(next_stop_occurrence(&session_dir, "agent-a").unwrap(), 2);
        assert_eq!(next_stop_occurrence(&session_dir, "agent-a").unwrap(), 3);
    }

    #[test]
    fn independent_counters_per_agent_in_one_session() {
        let home = TempHome::new().expect("temp home");
        let session_dir = session_dir(&home, "sess-1");
        assert_eq!(next_stop_occurrence(&session_dir, "agent-a").unwrap(), 1);
        assert_eq!(next_stop_occurrence(&session_dir, "agent-b").unwrap(), 1);
        assert_eq!(next_stop_occurrence(&session_dir, "agent-a").unwrap(), 2);
        assert_eq!(next_stop_occurrence(&session_dir, "agent-b").unwrap(), 2);
    }

    #[test]
    fn counters_are_independent_across_sessions() {
        let home = TempHome::new().expect("temp home");
        let sess_a = session_dir(&home, "sess-a");
        let sess_b = session_dir(&home, "sess-b");
        assert_eq!(next_stop_occurrence(&sess_a, "agent-x").unwrap(), 1);
        assert_eq!(next_stop_occurrence(&sess_b, "agent-x").unwrap(), 1);
    }

    #[test]
    fn corrupt_counter_file_is_a_hard_error_not_a_silent_reset() {
        let home = TempHome::new().expect("temp home");
        let session_dir = session_dir(&home, "sess-1");
        assert_eq!(next_stop_occurrence(&session_dir, "agent-a").unwrap(), 1);

        // Corrupt the data file directly.
        fs::write(session_dir.join(DATA_FILE_NAME), b"not json").unwrap();

        let err = next_stop_occurrence(&session_dir, "agent-a").unwrap_err();
        assert!(matches!(err, CounterError::Corrupt(_)));
    }

    #[test]
    fn data_and_lock_files_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let home = TempHome::new().expect("temp home");
        let session_dir = session_dir(&home, "sess-1");
        next_stop_occurrence(&session_dir, "agent-a").unwrap();

        for name in [LOCK_FILE_NAME, DATA_FILE_NAME] {
            let mode = fs::metadata(session_dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name} is 0600");
        }
    }
}
