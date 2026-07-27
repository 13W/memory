//! Durable, locked, rotating append to a session's LRSP segment (spec 07 §2).
//!
//! Segments live at `spool/<session_id>/<seq:06>.seg` (spec 02 §2,
//! `StoreLayout::spool_session`), rotate when the current one exceeds a
//! threshold (8 MiB default, spec 07 §2 `[SPEC]`), and are protected by an
//! exclusive advisory lock during append — `std::fs::File::lock()`, the same
//! portable (`flock` on unix / `LockFileEx` on Windows, no new dependency)
//! idiom `crates/store/src/migrate/lock.rs`'s `MigrationLock` already
//! establishes for the L1 migration lock.
//!
//! # The rotate-or-not decision happens *under* the lock
//!
//! `current_max_seq` is only an unlocked, best-effort starting guess (a plain
//! `read_dir` scan) — never load-bearing for correctness. The actual decision
//! — does the segment this call is about to open already exceed the rotation
//! threshold? — is only ever made from `file.metadata()` read **after**
//! `file.lock()` succeeds. This closes both races a naive unlocked check would
//! leave open:
//!
//! - *Two writers both deciding to rotate*: both must serialize on the same
//!   current segment's lock first (only one holds it at a time); whichever
//!   goes first re-derives the identical `seq + 1` from real, observed state,
//!   so both converge on the same next path rather than racing a shared
//!   counter. [`local_rag_core::paths::ensure_file_0600`]'s idempotent create
//!   means neither corrupts the other creating it.
//! - *Two writers both deciding not to rotate, one pushing past the
//!   threshold*: irrelevant by construction — an append that pushes the
//!   *current* segment over the threshold is allowed (the check is against
//!   the state *before* this write, mirroring spec 07 §2's own "opens a new
//!   segment when the current one **exceeds** 8 MiB"); the *next* writer's
//!   lock-then-check will see the now-larger size and rotate itself.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use local_rag_core::paths::{PathError, StoreLayout, ensure_dir, ensure_file_0600};
use local_rag_core::spool::encode_segment_header;

/// Default rotation threshold: "8 MiB" (spec 07 §2 `[SPEC]`). Callers may
/// inject a smaller value (e.g. in rotation tests) — see [`append_frame`].
pub const DEFAULT_ROTATE_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// Defensive bound on rotation retries within one [`append_frame`] call — real
/// runs need at most one extra iteration; this only guards against a
/// pathological loop.
const MAX_ROTATE_ATTEMPTS: u32 = 8;

/// A failure durably appending a frame to a session's spool.
#[derive(Debug)]
pub enum SpoolWriteError {
    /// A filesystem operation failed (create/open/lock/write/fsync).
    Io(io::Error),
    /// A store-path/permission error from the shared primitives.
    Path(PathError),
    /// Every segment tried within [`MAX_ROTATE_ATTEMPTS`] was already over the
    /// rotation threshold — a pathological state (or a rotation threshold of
    /// `0`), not a normal outcome.
    TooManyRotationAttempts,
}

impl std::fmt::Display for SpoolWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpoolWriteError::Io(e) => write!(f, "spool segment i/o error: {e}"),
            SpoolWriteError::Path(e) => write!(f, "spool segment path error: {e}"),
            SpoolWriteError::TooManyRotationAttempts => {
                write!(
                    f,
                    "exceeded the maximum number of segment rotation attempts"
                )
            }
        }
    }
}

impl std::error::Error for SpoolWriteError {}

impl From<io::Error> for SpoolWriteError {
    fn from(e: io::Error) -> Self {
        SpoolWriteError::Io(e)
    }
}

impl From<PathError> for SpoolWriteError {
    fn from(e: PathError) -> Self {
        SpoolWriteError::Path(e)
    }
}

/// Durably append `frame_bytes` (a complete `len‖crc32c‖payload` frame, see
/// [`local_rag_core::spool::encode_frame`]) to `session_id`'s current spool segment,
/// rotating to a new one first if the current segment already exceeds
/// `rotate_threshold_bytes`. A freshly created segment gets its 16-byte header
/// written in the same locked write as the frame (one `write_all` + one
/// `fdatasync`, matching spec 07 §2's "single write(O_APPEND) → fdatasync").
pub fn append_frame(
    layout: &StoreLayout,
    session_id: &str,
    frame_bytes: &[u8],
    rotate_threshold_bytes: u64,
) -> Result<(), SpoolWriteError> {
    let session_dir = layout.spool_session(session_id);
    ensure_dir(&session_dir)?;
    let mut seq = current_max_seq(&session_dir)?;

    for _ in 0..MAX_ROTATE_ATTEMPTS {
        let seg_path = segment_path(&session_dir, seq);
        ensure_file_0600(&seg_path)?;
        let mut file = OpenOptions::new().append(true).open(&seg_path)?;
        file.lock()?;

        let len = file.metadata()?.len();
        if len > rotate_threshold_bytes {
            let _ = file.unlock();
            drop(file);
            seq += 1;
            continue;
        }

        let mut out = Vec::with_capacity(HEADER_RESERVE + frame_bytes.len());
        if len == 0 {
            out.extend_from_slice(&encode_segment_header());
        }
        out.extend_from_slice(frame_bytes);
        file.write_all(&out)?;
        // Injection seam (feature-gated, zero-cost otherwise; spec 07 §7 S1/S2):
        // model a hard kill of the hook process after the write lands (already
        // durable via the OS page cache for a mere process kill, as opposed to
        // real power loss) but before `fdatasync` confirms it.
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!("hook.segment.after_write_before_fdatasync");
        file.sync_data()?;
        let _ = file.unlock();
        return Ok(());
    }
    Err(SpoolWriteError::TooManyRotationAttempts)
}

/// Capacity hint only (a header is 16 bytes; not a correctness constant).
const HEADER_RESERVE: usize = 16;

/// `spool/<session_id>/<seq:06>.seg` (spec 02 §2).
fn segment_path(session_dir: &Path, seq: u32) -> PathBuf {
    session_dir.join(format!("{seq:06}.seg"))
}

/// An unlocked, best-effort scan for the highest existing `<seq:06>.seg` in
/// `session_dir`; `1` (spec doesn't fix the first segment's number — this
/// task's `[SPEC]` pick) if none exist. Never load-bearing for correctness —
/// see the module-level race discussion.
fn current_max_seq(session_dir: &Path) -> Result<u32, SpoolWriteError> {
    let mut max_seq = 1u32;
    for entry in fs::read_dir(session_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(seq_str) = name.strip_suffix(".seg") else {
            continue;
        };
        let looks_like_seq = seq_str.len() == 6 && seq_str.bytes().all(|b| b.is_ascii_digit());
        if looks_like_seq && let Ok(seq) = seq_str.parse::<u32>() {
            max_seq = max_seq.max(seq);
        }
    }
    Ok(max_seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_test_support::TempHome;

    /// A `StoreLayout` with `spool/` pre-created — in production
    /// `StoreLayout::ensure()` creates it at store-init time (a precondition
    /// the whole daemon already relies on); `ensure_dir` itself only ever
    /// creates the session-specific leaf, never its ancestors.
    fn layout(home: &TempHome) -> StoreLayout {
        let l = StoreLayout::new(home.join("local-rag"));
        fs::create_dir_all(l.spool_dir()).expect("spool dir");
        l
    }

    #[test]
    fn first_write_creates_segment_one_with_header_and_frame() {
        let home = TempHome::new().expect("temp home");
        let l = layout(&home);
        let frame = local_rag_core::spool::encode_frame_bytes(b"{}").unwrap();
        append_frame(&l, "sess-1", &frame, DEFAULT_ROTATE_THRESHOLD_BYTES).unwrap();

        let seg_path = l.spool_session("sess-1").join("000001.seg");
        let bytes = fs::read(&seg_path).unwrap();
        assert_eq!(&bytes[..16], &encode_segment_header());
        assert_eq!(&bytes[16..], frame.as_slice());
    }

    #[test]
    fn second_write_appends_to_the_same_segment_under_the_threshold() {
        let home = TempHome::new().expect("temp home");
        let l = layout(&home);
        let frame_a = local_rag_core::spool::encode_frame_bytes(b"{\"a\":1}").unwrap();
        let frame_b = local_rag_core::spool::encode_frame_bytes(b"{\"b\":2}").unwrap();
        append_frame(&l, "sess-1", &frame_a, DEFAULT_ROTATE_THRESHOLD_BYTES).unwrap();
        append_frame(&l, "sess-1", &frame_b, DEFAULT_ROTATE_THRESHOLD_BYTES).unwrap();

        let session_dir = l.spool_session("sess-1");
        assert!(!session_dir.join("000002.seg").exists());
        let bytes = fs::read(session_dir.join("000001.seg")).unwrap();
        assert_eq!(&bytes[..16], &encode_segment_header());
        assert_eq!(&bytes[16..16 + frame_a.len()], frame_a.as_slice());
        assert_eq!(&bytes[16 + frame_a.len()..], frame_b.as_slice());
    }

    #[test]
    fn segment_paths_are_six_digit_zero_padded() {
        let session_dir = Path::new("/x");
        assert_eq!(segment_path(session_dir, 1), Path::new("/x/000001.seg"));
        assert_eq!(segment_path(session_dir, 42), Path::new("/x/000042.seg"));
    }

    #[test]
    fn current_max_seq_defaults_to_one_when_empty() {
        let home = TempHome::new().expect("temp home");
        let l = layout(&home);
        let session_dir = l.spool_session("sess-1");
        fs::create_dir_all(&session_dir).unwrap();
        assert_eq!(current_max_seq(&session_dir).unwrap(), 1);
    }

    #[test]
    fn current_max_seq_ignores_non_segment_files() {
        let home = TempHome::new().expect("temp home");
        let l = layout(&home);
        let session_dir = l.spool_session("sess-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("000001.seg"), b"").unwrap();
        fs::write(session_dir.join("000003.seg"), b"").unwrap();
        fs::write(session_dir.join(".subagent_stop_seq.json"), b"{}").unwrap();
        assert_eq!(current_max_seq(&session_dir).unwrap(), 3);
    }
}
