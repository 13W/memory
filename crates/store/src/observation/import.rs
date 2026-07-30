//! The transactional batch importer ([`import_batch`]) and per-session driver
//! ([`import_session_tail`]) that turn decoded LRSP frames into durable
//! observation rows (spec 03 §2.5, 07 §5/§6; T13-04). [`import_batch`] is the
//! first consumer of T13-03's decoder
//! (`local_rag_store::spool::{decode_segment, decode_frames}`).
//!
//! ## Scope boundary against `memory`
//!
//! Spec 03 §2.5 "Memory side" describes, in one SQL block, both the
//! observation ledger (owned here — [`super::SCHEMA_V7`]) and
//! `memory_entry`/`memory_evidence`/`pending_memory_candidate`/
//! `candidate_evidence`/`processing_cursor`/`consolidation_run`/`audit_event`
//! — [`crate::memory`]'s version-9 migration (T14-01, "Memory DDL and legal
//! transitions"). [`super::SCHEMA_V7`] creates only the four observation
//! tables; the memory tables are a separate, later migration.
//!
//! ## `RequestRoot` is injected, not computed
//!
//! Turning a frame's raw `worktree_root` (an uncanonicalized `cwd` string,
//! spec 07 §2's as-built note) into a [`RequestRoot`] requires git probing —
//! classifying `kind`, computing the common-dir/remote fingerprints — which
//! `registry::resolve`'s own module doc assigns to "the daemon's job (T15)":
//! `crates/store` carries no git dependency (architecture guardrail). Group
//! 15 (daemon/CLI) has not started. So [`import_batch`] and
//! [`import_session_tail`] accept an already-built `&RequestRoot` parameter —
//! today's callers (this task's own tests, and any driver built before group
//! 15 lands) pass `RequestRoot { worktree_root: None, repo_hint: None }`,
//! which [`resolve`] turns into [`Resolution::GlobalOnly`] — this **is** spec
//! 07 §5's "an unknown root imports with NULL worktree", not a stand-in for
//! it. A future group-15 driver passes real git-probed facts through the same
//! parameter without this module changing.
//!
//! ## Segment cleanup is not atomic with the commit
//!
//! Per the spool kill matrix (spec 07 §7 row S4: "daemon killed after tx
//! commit, before segment truncation ⇒ re-scan skips frames ≤ committed
//! offset"), deleting fully-consumed prior segment files happens **after**
//! the importing transaction commits, not inside it — [`import_session_tail`]
//! does this as a best-effort cleanup step; a crash between commit and
//! cleanup is harmless (the next pass re-derives the same "which segments are
//! now behind me" answer and deletes them then). The current segment is never
//! deleted or truncated in place.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Transaction;

use local_rag_core::hash::sha256_hex;
use local_rag_core::identity::UuidSource;
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::HeaderError;

use crate::registry::{RequestRoot, Resolution, resolve};
use crate::spool::{DecodedObservation, DedupClass, StopReason};
use crate::state::{OpenError, StateDb, WriteError};

use super::{
    NewObservationEnvelope, insert_envelope, insert_path, insert_payload, read_cursor,
    recent_same_source_event_exists, upsert_cursor,
};

/// Best-effort dedup window, time side (spec 07 §5 `[SPEC]`): 10 minutes.
const DEDUP_WINDOW_MS: i64 = 10 * 60 * 1000;
/// Best-effort dedup window, count side (spec 07 §5 `[SPEC]`): last 512
/// envelopes of the session.
const DEDUP_WINDOW_ENVELOPES: u32 = 512;

/// The outcome of importing one batch of already-decoded observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportBatchReport {
    /// New envelopes actually inserted (with their paths/payload, if any).
    pub imported: u32,
    /// Stable-identity events whose `dedup_key` already existed (spec 07 §5
    /// "UNIQUE(dedup_key) conflict ⇒ skip").
    pub exact_duplicates: u32,
    /// Best-effort events skipped by the bounded dedup window.
    pub window_duplicates: u32,
    /// A newly-imported (not deduplicated-away) `Stop` row was in this batch
    /// (spec 07 §6's "checkpoint on Stop" trigger, D-024).
    pub saw_stop: bool,
    /// As `saw_stop`, for `SessionEnd` (spec 07 §6's best-effort trigger).
    pub saw_session_end: bool,
}

/// Import `observations` (already decoded, in order) for `session_id` in one
/// transaction, and advance `spool_import_cursor` to
/// `(new_segment_seq, new_committed_offset)` in the same commit.
///
/// `observation_ids[i]` is the caller-minted UUIDv7 for `observations[i]`
/// (unused if that observation turns out to be a duplicate) — minted before
/// this call, outside the transaction, keeping entropy out of the write path
/// (the same discipline [`crate::create_repository`]'s caller follows).
/// `request_root` is resolved **once** for the whole batch (see this module's
/// doc) — every imported envelope gets the same `repo_id`/`worktree_id`.
/// `now_ms` is used only to compute `observation_payload.expires_at`;
/// nothing here reads the wall clock.
///
/// `evidence_kind`/`trust` are passed through as the frame's raw strings, not
/// pre-validated: the column's own `CHECK` constraint is the enforcement, so
/// an invalid value (a corrupted or forward-incompatible frame) surfaces as an
/// ordinary `rusqlite::Error`, rolling back the whole batch.
#[allow(clippy::too_many_arguments)]
pub fn import_batch(
    tx: &Transaction<'_>,
    session_id: &str,
    observations: &[DecodedObservation],
    observation_ids: &[String],
    request_root: &RequestRoot,
    now_ms: i64,
    payload_ttl_hours: u64,
    new_segment_seq: u32,
    new_committed_offset: u64,
) -> rusqlite::Result<ImportBatchReport> {
    debug_assert_eq!(observations.len(), observation_ids.len());

    let (repo_id, worktree_id) = match resolve(tx, request_root)? {
        Resolution::Resolved {
            repo_id,
            worktree_id,
        } => (Some(repo_id), Some(worktree_id)),
        Resolution::GlobalOnly | Resolution::Ambiguous { .. } => (None, None),
    };

    let mut report = ImportBatchReport::default();
    let expires_at = now_ms + payload_ttl_hours as i64 * 3_600_000;

    for (obs, observation_id) in observations.iter().zip(observation_ids) {
        let payload = &obs.payload;

        let dedup_key = match &obs.classification {
            DedupClass::Stable { dedup_key } => Some(dedup_key.as_str()),
            DedupClass::BestEffort => {
                let window_floor = payload.captured_at - DEDUP_WINDOW_MS;
                if recent_same_source_event_exists(
                    tx,
                    session_id,
                    &payload.source_event_id,
                    window_floor,
                    DEDUP_WINDOW_ENVELOPES,
                )? {
                    report.window_duplicates += 1;
                    continue;
                }
                None
            }
        };

        // The payload's own fingerprint (never an identity/UNIQUE/FK column,
        // spec 03 §2.5 doesn't mark it as one) — a plain hash, the same
        // "not domain-separated" choice T13-02 made for the best-effort
        // fingerprints (07 §4's as-built note): an empty byte slice for an
        // envelope-only event still hashes deterministically.
        let payload_hash = sha256_hex(payload.payload.as_deref().unwrap_or("").as_bytes());

        let row = NewObservationEnvelope {
            observation_id,
            source_event_id: &payload.source_event_id,
            dedup_key,
            payload_hash: &payload_hash,
            event_type: &payload.event_type,
            evidence_kind: &payload.evidence_kind,
            trust: &payload.trust,
            source_timestamp: Some(payload.captured_at),
            repo_id: repo_id.as_deref(),
            worktree_id: worktree_id.as_deref(),
            session_id,
            agent_id: payload.agent_id.as_deref(),
            turn_id: payload.turn_id.as_deref(),
            batch_id: payload.batch_id.as_deref(),
            commit_hash: payload.commit.as_deref(),
            short_evidence_excerpt: payload.short_evidence_excerpt.as_deref(),
            redaction_version: payload.redaction_version.map(i64::from),
        };

        let Some(_received_seq) = insert_envelope(tx, &row)? else {
            // Stable dedup_key already imported (spec 07 §5 exact-dedup path).
            report.exact_duplicates += 1;
            continue;
        };

        for path in &payload.paths {
            insert_path(tx, observation_id, path)?;
        }
        if let Some(text) = &payload.payload {
            insert_payload(tx, observation_id, text.as_bytes(), expires_at)?;
        }
        report.imported += 1;
        match payload.event_type.as_str() {
            "Stop" => report.saw_stop = true,
            "SessionEnd" => report.saw_session_end = true,
            _ => {}
        }
    }

    upsert_cursor(
        tx,
        session_id,
        new_segment_seq,
        new_committed_offset,
        now_ms,
    )?;

    // Injection seam (feature-gated, zero-cost otherwise; spec 07 §7 S3):
    // model a hard kill of the daemon after every row is staged but before
    // this closure returns — `StateWriter::transaction`'s `txn.commit()` runs
    // only *after* a successful return, so the process dies with the
    // transaction still open, and it rolls back exactly as a crash would
    // leave it.
    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!("observation.import_batch.before_commit");

    Ok(report)
}

/// The outcome of one [`import_session_tail`] pass.
#[derive(Debug)]
pub struct ImportOutcome {
    pub report: ImportBatchReport,
    /// The cursor's `segment_seq` after this pass (unchanged if nothing new
    /// was available).
    pub final_segment_seq: u32,
    /// The cursor's `committed_offset` within `final_segment_seq` after this
    /// pass.
    pub final_committed_offset: u64,
    /// `Some(description)` if decoding stopped on genuine corruption (not a
    /// torn tail, not simply "no more data yet") — the cursor does **not**
    /// advance past the corrupt byte range, so this stalls future imports of
    /// this session until an operator investigates. Described via `Display`
    /// (`local_rag_store::spool::FrameDecodeError` is not `Clone`), not
    /// silently retried with resync heuristics.
    pub stalled_on: Option<String>,
}

/// A failure importing a session's spool tail.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImportError {
    /// Opening the read-only state connection (to read the cursor) failed.
    Open(OpenError),
    /// Reading the current cursor failed.
    Sqlite(rusqlite::Error),
    /// A filesystem operation (reading or deleting a segment file) failed.
    Io(std::io::Error),
    /// The importing transaction failed (rolled back; the store is unchanged).
    Write(WriteError),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Open(e) => write!(f, "could not open the state store: {e}"),
            ImportError::Sqlite(e) => write!(f, "could not read the import cursor: {e}"),
            ImportError::Io(e) => write!(f, "spool segment i/o error: {e}"),
            ImportError::Write(e) => write!(f, "importing observations failed: {e}"),
        }
    }
}

impl std::error::Error for ImportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ImportError::Open(e) => Some(e),
            ImportError::Sqlite(e) => Some(e),
            ImportError::Io(e) => Some(e),
            ImportError::Write(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        ImportError::Io(e)
    }
}

/// `spool/<session_id>/<seq:06>.seg` (spec 02 §2), mirroring
/// `local_rag_hook::segment`'s own private helper of the same shape.
fn segment_path(session_dir: &Path, seq: u32) -> PathBuf {
    session_dir.join(format!("{seq:06}.seg"))
}

/// Read, decode, and import as much of `session_id`'s un-imported spool tail
/// as is currently available, in one transaction, then (best-effort, outside
/// the transaction) delete fully-consumed prior segment files.
///
/// Reads the current cursor (absent ⇒ start at segment 1, offset 0), then
/// walks forward: decodes frames from the current segment starting at the
/// cursor's offset; on a clean end of that segment's data, continues into the
/// next segment file if it already exists on disk, otherwise stops (nothing
/// more to read yet); stops at a torn tail or a truncated header (both
/// normal — the writer has not finished this frame/segment yet) or at genuine
/// corruption (bad magic, an unsupported format version, a bad CRC/length/
/// UTF-8/shape — reported via `stalled_on`, cursor left just before it, never
/// silently skipped past). Every observation decoded across however many
/// segments this pass covers is imported in one [`import_batch`] call, and
/// `observation_id`s are minted before that call (keeping entropy out of the
/// write path).
pub async fn import_session_tail(
    db: &StateDb,
    layout: &StoreLayout,
    session_id: &str,
    request_root: &RequestRoot,
    uuids: &(dyn UuidSource + Send + Sync),
    now_ms: i64,
    payload_ttl_hours: u64,
) -> Result<ImportOutcome, ImportError> {
    let (start_segment_seq, start_offset) = {
        let read = db.open_read().map_err(ImportError::Open)?;
        read_cursor(&read, session_id).map_err(ImportError::Sqlite)?
    }
    .unwrap_or((1, 0));

    let session_dir = layout.spool_session(session_id);
    let mut segment_seq = start_segment_seq;
    let mut offset = start_offset;
    let mut observations: Vec<DecodedObservation> = Vec::new();
    let mut stalled_on: Option<String> = None;

    loop {
        let seg_path = segment_path(&session_dir, segment_seq);
        if !seg_path.exists() {
            break; // Nothing more to read yet.
        }
        let bytes = fs::read(&seg_path)?;
        let header = match local_rag_core::spool::decode_segment_header(&bytes) {
            Ok(h) => h,
            // A torn write of a brand-new segment's header+first-frame
            // combined write (spec 07 §2's single write(O_APPEND)) — the
            // writer has not finished yet, not corruption.
            Err(HeaderError::Truncated) => break,
            Err(e) => {
                stalled_on = Some(format!("segment {segment_seq}: {e}"));
                break;
            }
        };
        let start = offset.max(local_rag_core::spool::HEADER_LEN as u64) as usize;
        let start = start.min(bytes.len());
        let decoded = crate::spool::decode_frames(&bytes[start..], header.version);
        offset = (start + decoded.bytes_consumed) as u64;
        observations.extend(decoded.frames);

        match decoded.stop_reason {
            StopReason::EndOfInput => {
                if segment_path(&session_dir, segment_seq + 1).exists() {
                    segment_seq += 1;
                    offset = 0;
                    continue;
                }
                break;
            }
            StopReason::TornTail => break,
            StopReason::Corrupt(e) => {
                stalled_on = Some(format!("segment {segment_seq} offset {start}: {e}"));
                break;
            }
        }
    }

    let report =
        if observations.is_empty() && segment_seq == start_segment_seq && offset == start_offset {
            ImportBatchReport::default()
        } else {
            let observation_ids: Vec<String> = observations
                .iter()
                .map(|_| uuids.next_uuid().to_string())
                .collect();
            let session_id_owned = session_id.to_string();
            let request_root_owned = request_root.clone();
            let final_segment_seq = segment_seq;
            let final_offset = offset;
            db.writer()
                .transaction(move |tx| {
                    import_batch(
                        tx,
                        &session_id_owned,
                        &observations,
                        &observation_ids,
                        &request_root_owned,
                        now_ms,
                        payload_ttl_hours,
                        final_segment_seq,
                        final_offset,
                    )
                })
                .await
                .map_err(ImportError::Write)?
        };

    // Injection seam (feature-gated, zero-cost otherwise; spec 07 §7 S4):
    // model a hard kill right after the importing transaction durably
    // commits but before any segment cleanup below has run.
    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "observation.import_session_tail.after_commit_before_cleanup"
    );

    // Best-effort cleanup, outside the transaction (see module doc): every
    // segment strictly behind the cursor's *current* segment is fully
    // consumed, whether or not this particular pass is the one that walked
    // through it — a prior pass may have advanced the cursor and then been
    // killed before finishing its own cleanup (spec 07 §7 S4/S5), so this
    // always sweeps from scratch rather than only the delta this pass saw,
    // or a leftover segment could linger forever undetected.
    for seq in 1..segment_seq {
        let seg = segment_path(&session_dir, seq);
        if seg.exists() {
            let _ = fs::remove_file(&seg);
        }
        // Injection seam (feature-gated, zero-cost otherwise; spec 07 §7 S5):
        // model a hard kill partway through cleanup, after this segment was
        // handled but before the next one is — the already-committed DB state
        // does not depend on this loop finishing.
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!("observation.import_session_tail.mid_cleanup");
    }

    Ok(ImportOutcome {
        report,
        final_segment_seq: segment_seq,
        final_committed_offset: offset,
        stalled_on,
    })
}

/// Every session with a spool directory under `layout`'s `spool/` root (T13-05:
/// the daemon-startup catch-up seam, spec 07 §6 "catch-up of unprocessed
/// observations at daemon startup").
///
/// A thin enumeration only — each returned `session_id` is meant to be passed
/// to [`import_session_tail`] by whichever future caller implements the actual
/// startup/periodic scheduling (a background worker, group 15's daemon
/// lifecycle); this module ships the seam, not the scheduler, the same
/// deferral every sweep in `crate::housekeeping` already carries. An absent
/// `spool/` directory yields an empty list rather than an error (a store that
/// has never seen a hook write).
pub fn known_spool_sessions(layout: &StoreLayout) -> std::io::Result<Vec<String>> {
    let mut sessions = Vec::new();
    let entries = match fs::read_dir(layout.spool_dir()) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            sessions.push(name.to_string());
        }
    }
    sessions.sort();
    Ok(sessions)
}
