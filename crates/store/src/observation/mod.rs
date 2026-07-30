//! Spool-derived observation tables in `state.sqlite` (spec 03 §2.5 "Memory
//! side" — the observation-ledger subset only; see [`import`]'s module doc for
//! the scope boundary against [`crate::memory`]'s memory-entry tables).
//!
//! This module owns the seventh numbered migration ([`SCHEMA_V7`]): the four
//! tables a decoded LRSP frame (`local_rag_store::spool::DecodedObservation`,
//! T13-03) becomes durable rows in — `observation_envelope`, `observation_path`,
//! `observation_payload`, and `spool_import_cursor` — plus the low-level typed
//! row inserts and cursor read/write. The transactional batch importer that
//! composes them into one commit (dedup, resolution, cursor advance) is
//! [`import::import_batch`]; the per-session driver that reads real segment
//! files off disk is [`import::import_session_tail`] (T13-04).
//!
//! Following the registry/code convention, write operations take a
//! [`Transaction`] so they compose inside a single
//! [`StateWriter::transaction`](crate::StateWriter::transaction) closure; read
//! operations take a [`Connection`]. `observation_id` is minted by the caller
//! (a UUIDv7 from [`local_rag_core::identity::uuidv7`]) and passed in, keeping
//! entropy out of the write path, mirroring [`create_repository`](crate::create_repository).
//!
//! T13-05 adds the **payload TTL sweep** ([`payload_ttl::run_payload_ttl_sweep`],
//! spec 12 §3) and the **startup catch-up enumeration seam**
//! ([`import::known_spool_sessions`], spec 07 §6); the companion **spool
//! session GC** sweep lives in [`crate::housekeeping`] alongside its filesystem
//! sweep siblings.

mod import;
mod payload_ttl;

pub use import::{
    ImportBatchReport, ImportError, ImportOutcome, import_batch, import_session_tail,
    known_spool_sessions,
};
pub use payload_ttl::{PayloadSweepError, PayloadSweepReport, run_payload_ttl_sweep};

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

/// Version-7 migration DDL: the spool-derived observation ledger (spec 03
/// §2.5, the `observation_envelope`/`observation_path`/`observation_payload`/
/// `spool_import_cursor` subset — see [`import`]'s module doc for why the
/// remaining §2.5 tables are **not** here). Referenced by
/// [`crate::migrate::ALL`] as migration version 7.
///
/// **Frozen once shipped.** Like the earlier `SCHEMA_V*` constants, the
/// checksum is the SHA-256 of this text (see
/// [`crate::migrate::Migration::checksum`]); any edit trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an
/// existing store. Future schema changes are new numbered migrations.
pub(crate) const SCHEMA_V7: &str = "\
CREATE TABLE observation_envelope (
  received_seq      INTEGER PRIMARY KEY AUTOINCREMENT,
  observation_id    TEXT NOT NULL UNIQUE,
  source_event_id   TEXT NOT NULL,
  dedup_key         TEXT,
  payload_hash      TEXT NOT NULL,
  event_type        TEXT NOT NULL,
  evidence_kind     TEXT NOT NULL CHECK
    (evidence_kind IN ('user_statement','tool_result','test_result','code_state','model_claim')),
  trust             TEXT NOT NULL CHECK (trust IN ('low','normal','high')),
  source_timestamp  INTEGER,
  repo_id           TEXT REFERENCES repository(repo_id),
  worktree_id       TEXT REFERENCES worktree(worktree_id),
  session_id        TEXT NOT NULL,
  agent_id          TEXT,
  turn_id           TEXT,
  batch_id          TEXT,
  commit_hash       TEXT,
  short_evidence_excerpt TEXT
);
CREATE UNIQUE INDEX envelope_dedup
  ON observation_envelope(dedup_key) WHERE dedup_key IS NOT NULL;
CREATE INDEX envelope_session ON observation_envelope(session_id, received_seq);

CREATE TABLE observation_path (
  observation_id   TEXT NOT NULL REFERENCES observation_envelope(observation_id) ON DELETE CASCADE,
  normalized_path  TEXT NOT NULL,
  PRIMARY KEY (observation_id, normalized_path)
);

CREATE TABLE observation_payload (
  observation_id   TEXT PRIMARY KEY REFERENCES observation_envelope(observation_id) ON DELETE CASCADE,
  redacted_payload BLOB NOT NULL,
  byte_size        INTEGER NOT NULL,
  expires_at       INTEGER NOT NULL
);

CREATE TABLE spool_import_cursor (
  session_id        TEXT PRIMARY KEY,
  segment_seq       INTEGER NOT NULL,
  committed_offset  INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL
);
";

/// Version-8 migration DDL (D-019): adds `observation_envelope.redaction_version`
/// (spec 12 §2 `[SPEC]` "versioned `redaction_version` recorded in envelopes"),
/// closing a gap found at gate G13 — the value was computed at write time
/// (`local_rag_hook::payload::prepare_payload`) but discarded before it ever
/// reached the wire format or this table. No backfill: unlike D-007's
/// `state_changed_at` (where a `0` default would have been actively wrong),
/// `NULL` is the correct, legitimate value both for rows written before this
/// migration and for an envelope-only (denied) event, whose payload was never
/// scanned in the first place. Referenced by [`crate::migrate::ALL`] as
/// migration version 8.
///
/// **Frozen once shipped.** Like the earlier `SCHEMA_V*` constants, the
/// checksum is the SHA-256 of this text (see
/// [`crate::migrate::Migration::checksum`]); any edit trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an
/// existing store. Future schema changes are new numbered migrations.
pub(crate) const SCHEMA_V8: &str = "\
ALTER TABLE observation_envelope ADD COLUMN redaction_version INTEGER;
";

/// `observation_envelope.evidence_kind` (spec 03 §2.5's CHECK domain; spec 07
/// §2's as-built note on `local_rag_hook::identity::evidence_kind_and_trust`
/// names the same five values as free-form strings at write time — this is
/// the read-side typed mirror).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    UserStatement,
    ToolResult,
    TestResult,
    CodeState,
    ModelClaim,
}

impl EvidenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceKind::UserStatement => "user_statement",
            EvidenceKind::ToolResult => "tool_result",
            EvidenceKind::TestResult => "test_result",
            EvidenceKind::CodeState => "code_state",
            EvidenceKind::ModelClaim => "model_claim",
        }
    }

    /// Parse a stored/frame value; `None` for anything the CHECK domain forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "user_statement" => Some(EvidenceKind::UserStatement),
            "tool_result" => Some(EvidenceKind::ToolResult),
            "test_result" => Some(EvidenceKind::TestResult),
            "code_state" => Some(EvidenceKind::CodeState),
            "model_claim" => Some(EvidenceKind::ModelClaim),
            _ => None,
        }
    }
}

/// `observation_envelope.trust` (spec 03 §2.5's CHECK domain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    Low,
    Normal,
    High,
}

impl TrustLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustLevel::Low => "low",
            TrustLevel::Normal => "normal",
            TrustLevel::High => "high",
        }
    }

    /// Parse a stored/frame value; `None` for anything the CHECK domain forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "low" => Some(TrustLevel::Low),
            "normal" => Some(TrustLevel::Normal),
            "high" => Some(TrustLevel::High),
            _ => None,
        }
    }
}

/// A new `observation_envelope` row, mirroring the DDL 1:1 (see
/// [`code::revision::NewFileRevision`](crate::code) for the same `New<Table>`
/// convention). `observation_id` is caller-minted (UUIDv7); `repo_id`/
/// `worktree_id` are the caller's already-resolved identity (see
/// [`import`]'s module doc on why resolution is injected, not computed here).
#[derive(Debug, Clone, Copy)]
pub struct NewObservationEnvelope<'a> {
    pub observation_id: &'a str,
    pub source_event_id: &'a str,
    /// `Some` only for a stable-identity event (spec 07 §4); `None` is never a
    /// UNIQUE-index match (the partial index excludes NULLs), so a best-effort
    /// row can never spuriously conflict here.
    pub dedup_key: Option<&'a str>,
    pub payload_hash: &'a str,
    pub event_type: &'a str,
    /// Raw frame string, deliberately **not** pre-parsed into [`EvidenceKind`]
    /// here: the column's own `CHECK` constraint is the enforcement, so an
    /// invalid value (a corrupted or forward-incompatible frame) surfaces as an
    /// ordinary `rusqlite::Error` that rolls back the whole batch, rather than
    /// this module inventing a second, redundant validation path. [`EvidenceKind`]
    /// exists for typed reads (a future consumer), not as a write-time gate.
    pub evidence_kind: &'a str,
    /// See [`NewObservationEnvelope::evidence_kind`] — same reasoning, [`TrustLevel`].
    pub trust: &'a str,
    pub source_timestamp: Option<i64>,
    pub repo_id: Option<&'a str>,
    pub worktree_id: Option<&'a str>,
    pub session_id: &'a str,
    pub agent_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub batch_id: Option<&'a str>,
    pub commit_hash: Option<&'a str>,
    pub short_evidence_excerpt: Option<&'a str>,
    /// The redaction scanner version that produced this event's payload
    /// (spec 12 §2 `[SPEC]`, D-019); `None` for an envelope-only (denied)
    /// event, whose payload was never scanned, and for a frame written before
    /// migration 8 existed.
    pub redaction_version: Option<i64>,
}

/// Insert one `observation_envelope` row, returning its assigned
/// `received_seq` — or `None` if a stable `dedup_key` already exists (spec 07
/// §5 "UNIQUE(dedup_key) conflict ⇒ skip", the exact-dedup path). A `None`
/// `dedup_key` (best-effort events) can never trigger this conflict; the
/// bounded-window dedup check for those is a separate read
/// ([`recent_same_source_event_exists`]) the caller runs *before* calling this.
///
/// `pub` (T15-05, widened from `pub(crate)`): [`import`]'s `import_batch` is
/// not the only legitimate caller — it is shaped for the spool decoder
/// (cursor advance, batch of `DecodedObservation`), machinery a single
/// daemon-internal write (the MCP `give_feedback` tool, spec 11 §2: "writes
/// an observation envelope directly... spool-only constraint applies to
/// hooks, not to daemon-internal writes") has no use for. This primitive
/// itself has no spool coupling at all, so widening it is the direct reuse,
/// not a new wrapper.
pub fn insert_envelope(
    tx: &Transaction<'_>,
    row: &NewObservationEnvelope<'_>,
) -> rusqlite::Result<Option<i64>> {
    tx.query_row(
        "INSERT INTO observation_envelope \
           (observation_id, source_event_id, dedup_key, payload_hash, event_type, \
            evidence_kind, trust, source_timestamp, repo_id, worktree_id, session_id, \
            agent_id, turn_id, batch_id, commit_hash, short_evidence_excerpt, \
            redaction_version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) \
         ON CONFLICT(dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING \
         RETURNING received_seq",
        params![
            row.observation_id,
            row.source_event_id,
            row.dedup_key,
            row.payload_hash,
            row.event_type,
            row.evidence_kind,
            row.trust,
            row.source_timestamp,
            row.repo_id,
            row.worktree_id,
            row.session_id,
            row.agent_id,
            row.turn_id,
            row.batch_id,
            row.commit_hash,
            row.short_evidence_excerpt,
            row.redaction_version,
        ],
        |r| r.get(0),
    )
    .optional()
}

/// Insert one `observation_path` row for an already-inserted envelope.
/// `INSERT OR IGNORE`: the frame's own `paths` list is not guaranteed
/// duplicate-free, and the composite primary key already de-duplicates.
pub(crate) fn insert_path(
    tx: &Transaction<'_>,
    observation_id: &str,
    normalized_path: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO observation_path (observation_id, normalized_path) \
         VALUES (?1, ?2)",
        params![observation_id, normalized_path],
    )?;
    Ok(())
}

/// Insert one `observation_payload` row for an already-inserted envelope.
/// Never called for an envelope-only (denied/redacted-away) event — the
/// absence of a row *is* "no payload", not an expired one (spec 03 §2.5
/// `observation_payload` doc: "short TTL; envelope survives it").
pub(crate) fn insert_payload(
    tx: &Transaction<'_>,
    observation_id: &str,
    redacted_payload: &[u8],
    expires_at: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO observation_payload \
           (observation_id, redacted_payload, byte_size, expires_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            observation_id,
            redacted_payload,
            redacted_payload.len() as i64,
            expires_at,
        ],
    )?;
    Ok(())
}

/// Whether an envelope with the same `source_event_id` already exists for
/// `session_id`, within the bounded best-effort dedup window (spec 07 §5:
/// "received within `[SPEC]` 10 min / last `[SPEC]` 512 envelopes").
///
/// As-built `[SPEC]` interpretation (this task): the two bounds are a union
/// (`OR`), the same "most protective" reading `retention::mark_pins`'s K/T
/// window already established for retiring generations — a candidate counts
/// as a duplicate if it is within the last 512 envelopes *of this session*
/// **or** within 10 minutes of `now_reference_ms` (the new frame's own
/// `captured_at`, not the wall clock — see [`import::import_batch`]'s doc for
/// why). "Last 512 of this session" is computed by session-scoped rank, not a
/// raw `received_seq` range, because `received_seq` is one global sequence
/// shared by every session.
pub(crate) fn recent_same_source_event_exists(
    conn: &Connection,
    session_id: &str,
    source_event_id: &str,
    window_floor_ms: i64,
    window_envelopes: u32,
) -> rusqlite::Result<bool> {
    let offset = (window_envelopes.max(1) - 1) as i64;
    conn.query_row(
        "WITH threshold AS ( \
           SELECT received_seq AS floor_seq FROM observation_envelope \
           WHERE session_id = ?1 ORDER BY received_seq DESC LIMIT 1 OFFSET ?4 \
         ) \
         SELECT EXISTS ( \
           SELECT 1 FROM observation_envelope \
           WHERE session_id = ?1 AND source_event_id = ?2 \
             AND ( \
               (source_timestamp IS NOT NULL AND source_timestamp >= ?3) \
               OR received_seq >= COALESCE((SELECT floor_seq FROM threshold), -1) \
             ) \
         )",
        params![session_id, source_event_id, window_floor_ms, offset],
        |r| r.get(0),
    )
}

/// The current `(segment_seq, committed_offset)` cursor for `session_id`, or
/// `None` if this session has never been imported before (the driver then
/// starts at the beginning of segment 1).
pub(crate) fn read_cursor(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<(u32, u64)>> {
    conn.query_row(
        "SELECT segment_seq, committed_offset FROM spool_import_cursor WHERE session_id = ?1",
        params![session_id],
        |r| Ok((r.get::<_, i64>(0)? as u32, r.get::<_, i64>(1)? as u64)),
    )
    .optional()
}

/// Upsert the `(segment_seq, committed_offset)` cursor for `session_id`.
pub(crate) fn upsert_cursor(
    tx: &Transaction<'_>,
    session_id: &str,
    segment_seq: u32,
    committed_offset: u64,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO spool_import_cursor (session_id, segment_seq, committed_offset, updated_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(session_id) DO UPDATE SET \
           segment_seq = excluded.segment_seq, \
           committed_offset = excluded.committed_offset, \
           updated_at = excluded.updated_at",
        params![session_id, segment_seq, committed_offset as i64, now_ms],
    )?;
    Ok(())
}

/// One session's `spool_import_cursor` row (T13-05: the spool session GC sweep,
/// `crate::housekeeping::run_spool_session_sweep`, reads every session's cursor
/// to decide absence/full-commit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpoolCursorRow {
    pub session_id: String,
    pub segment_seq: u32,
    pub committed_offset: u64,
    pub updated_at: i64,
}

/// Every session's current cursor row, unordered.
pub(crate) fn all_cursors(conn: &Connection) -> rusqlite::Result<Vec<SpoolCursorRow>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, segment_seq, committed_offset, updated_at FROM spool_import_cursor",
    )?;
    stmt.query_map([], |r| {
        Ok(SpoolCursorRow {
            session_id: r.get(0)?,
            segment_seq: r.get::<_, i64>(1)? as u32,
            committed_offset: r.get::<_, i64>(2)? as u64,
            updated_at: r.get(3)?,
        })
    })?
    .collect()
}

/// Delete `session_id`'s cursor row (T13-05: after its spool directory has been
/// GC'd — spec 07 §6 — so no orphaned cursor row lingers).
pub(crate) fn delete_cursor(tx: &Transaction<'_>, session_id: &str) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM spool_import_cursor WHERE session_id = ?1",
        params![session_id],
    )?;
    Ok(())
}

/// The highest `received_seq` recorded for `session_id`, or `None` if the
/// session has never appended an envelope (T14-06: bounds a consolidation
/// run's snapshot, spec 08 §4 step 1 — `to_received_seq = min(cursor+batch,
/// max_seq)`). `SELECT MAX(...)` always returns exactly one row even over
/// zero matching envelopes, with a `NULL` aggregate — no `.optional()` needed.
pub(crate) fn max_received_seq(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(received_seq) FROM observation_envelope WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )
}

/// One envelope inside a consolidation window, plus its still-live payload if
/// any (T14-06, spec 08 §4 step 2: "Load envelopes (+ surviving payloads) of
/// the window"). `payload: None` is the normal case for a payload the TTL
/// sweep already removed ([`payload_ttl`]) or an envelope-only (denied/
/// redacted-away) event — never an error, mirroring `observation_payload`'s
/// own DDL comment ("short TTL; envelope survives it").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowEnvelopeRow {
    pub received_seq: i64,
    pub observation_id: String,
    pub event_type: String,
    pub evidence_kind: EvidenceKind,
    pub trust: TrustLevel,
    /// Which repository/worktree this observation was captured against, if
    /// any (T14-07: the router resolves a `create`/`supersede`'s
    /// `scope_kind=repository|worktree` target from this — see
    /// `local_rag_memory::recall`'s own doc for how a window with more than
    /// one distinct value is handled).
    pub repo_id: Option<String>,
    pub worktree_id: Option<String>,
    pub agent_id: Option<String>,
    pub commit_hash: Option<String>,
    pub short_evidence_excerpt: Option<String>,
    pub payload: Option<Vec<u8>>,
}

/// Every envelope for `session_id` with `received_seq` in
/// `[from_received_seq, to_received_seq]` (both ends inclusive), ascending —
/// T14-06's window load (spec 08 §4 step 2), left-joined against any
/// still-live `observation_payload` row so a swept/absent payload reads back
/// as `WindowEnvelopeRow::payload == None` rather than shortening the result.
pub(crate) fn envelopes_in_range(
    conn: &Connection,
    session_id: &str,
    from_received_seq: i64,
    to_received_seq: i64,
) -> rusqlite::Result<Vec<WindowEnvelopeRow>> {
    let mut stmt = conn.prepare(
        "SELECT e.received_seq, e.observation_id, e.event_type, e.evidence_kind, e.trust, \
                e.repo_id, e.worktree_id, e.agent_id, e.commit_hash, e.short_evidence_excerpt, \
                p.redacted_payload \
         FROM observation_envelope e \
         LEFT JOIN observation_payload p ON p.observation_id = e.observation_id \
         WHERE e.session_id = ?1 AND e.received_seq BETWEEN ?2 AND ?3 \
         ORDER BY e.received_seq",
    )?;
    let rows = stmt
        .query_map(
            params![session_id, from_received_seq, to_received_seq],
            |r| {
                let raw_evidence_kind: String = r.get(3)?;
                let evidence_kind = EvidenceKind::from_db(&raw_evidence_kind).ok_or_else(|| {
                    Error::FromSqlConversionFailure(
                        3,
                        Type::Text,
                        format!("invalid observation_envelope.evidence_kind {raw_evidence_kind:?}")
                            .into(),
                    )
                })?;
                let raw_trust: String = r.get(4)?;
                let trust = TrustLevel::from_db(&raw_trust).ok_or_else(|| {
                    Error::FromSqlConversionFailure(
                        4,
                        Type::Text,
                        format!("invalid observation_envelope.trust {raw_trust:?}").into(),
                    )
                })?;
                Ok(WindowEnvelopeRow {
                    received_seq: r.get(0)?,
                    observation_id: r.get(1)?,
                    event_type: r.get(2)?,
                    evidence_kind,
                    trust,
                    repo_id: r.get(5)?,
                    worktree_id: r.get(6)?,
                    agent_id: r.get(7)?,
                    commit_hash: r.get(8)?,
                    short_evidence_excerpt: r.get(9)?,
                    payload: r.get(10)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_core::paths::StoreLayout;
    use local_rag_test_support::TempHome;

    fn open_state() -> (TempHome, crate::StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = crate::StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, db)
    }

    /// A `NewObservationEnvelope` with every field but the ones under test
    /// filled with harmless, valid defaults.
    fn row<'a>(
        observation_id: &'a str,
        session_id: &'a str,
        source_event_id: &'a str,
        dedup_key: Option<&'a str>,
        source_timestamp: Option<i64>,
    ) -> NewObservationEnvelope<'a> {
        NewObservationEnvelope {
            observation_id,
            source_event_id,
            dedup_key,
            payload_hash: "deadbeef",
            event_type: "Stop",
            evidence_kind: "model_claim",
            trust: "low",
            source_timestamp,
            repo_id: None,
            worktree_id: None,
            session_id,
            agent_id: None,
            turn_id: None,
            batch_id: None,
            commit_hash: None,
            short_evidence_excerpt: None,
            redaction_version: None,
        }
    }

    #[test]
    fn evidence_kind_round_trips_and_rejects_unknown() {
        for kind in [
            EvidenceKind::UserStatement,
            EvidenceKind::ToolResult,
            EvidenceKind::TestResult,
            EvidenceKind::CodeState,
            EvidenceKind::ModelClaim,
        ] {
            assert_eq!(EvidenceKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(EvidenceKind::from_db("bogus"), None);
    }

    #[test]
    fn trust_level_round_trips_and_rejects_unknown() {
        for trust in [TrustLevel::Low, TrustLevel::Normal, TrustLevel::High] {
            assert_eq!(TrustLevel::from_db(trust.as_str()), Some(trust));
        }
        assert_eq!(TrustLevel::from_db("bogus"), None);
    }

    #[tokio::test]
    async fn insert_envelope_path_and_payload_round_trip() {
        let (_home, db) = open_state();
        let received_seq = db
            .writer()
            .transaction(move |tx| {
                let seq = insert_envelope(
                    tx,
                    &row("obs-1", "sess-1", "st:sess-1:x:1", None, Some(1000)),
                )?
                .expect("first insert always succeeds");
                insert_path(tx, "obs-1", "src/a.rs")?;
                insert_path(tx, "obs-1", "src/a.rs")?; // duplicate path is ignored, not an error.
                insert_payload(tx, "obs-1", b"{\"x\":1}", 5000)?;
                Ok(seq)
            })
            .await
            .expect("transaction commits");
        assert_eq!(received_seq, 1);

        let read = db.open_read().expect("read conn");
        let paths: i64 = read
            .query_row(
                "SELECT count(*) FROM observation_path WHERE observation_id = 'obs-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(paths, 1, "duplicate path collapsed by the composite PK");
        let (byte_size, expires_at): (i64, i64) = read
            .query_row(
                "SELECT byte_size, expires_at FROM observation_payload WHERE observation_id = 'obs-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(byte_size, 7);
        assert_eq!(expires_at, 5000);
    }

    /// D-019: `redaction_version` round-trips through `insert_envelope`, and a
    /// row with none (the envelope-only/denied path, or a frame written before
    /// migration 8) reads back as `NULL`, not some fabricated default.
    #[tokio::test]
    async fn insert_envelope_round_trips_redaction_version() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(move |tx| {
                insert_envelope(
                    tx,
                    &NewObservationEnvelope {
                        redaction_version: Some(1),
                        ..row("obs-with", "sess-1", "st:sess-1:x:1", None, Some(1000))
                    },
                )?;
                insert_envelope(
                    tx,
                    &row("obs-without", "sess-1", "st:sess-1:y:2", None, Some(2000)),
                )?;
                Ok(())
            })
            .await
            .expect("transaction commits");

        let read = db.open_read().expect("read conn");
        let with_version: Option<i64> = read
            .query_row(
                "SELECT redaction_version FROM observation_envelope WHERE observation_id = 'obs-with'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(with_version, Some(1));
        let without_version: Option<i64> = read
            .query_row(
                "SELECT redaction_version FROM observation_envelope WHERE observation_id = 'obs-without'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(without_version, None);
    }

    #[tokio::test]
    async fn insert_envelope_stable_dedup_key_conflict_returns_none() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(move |tx| {
                let first = insert_envelope(
                    tx,
                    &row(
                        "obs-1",
                        "sess-1",
                        "pt:sess-1:t1:ok",
                        Some("pt:sess-1:t1:ok"),
                        Some(1),
                    ),
                )?;
                assert!(first.is_some());
                let second = insert_envelope(
                    tx,
                    &row(
                        "obs-2",
                        "sess-1",
                        "pt:sess-1:t1:ok",
                        Some("pt:sess-1:t1:ok"),
                        Some(2),
                    ),
                )?;
                assert!(
                    second.is_none(),
                    "same dedup_key must not insert a second row"
                );
                Ok(())
            })
            .await
            .expect("transaction commits");

        let read = db.open_read().expect("read conn");
        let count: i64 = read
            .query_row("SELECT count(*) FROM observation_envelope", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn cursor_defaults_to_absent_then_round_trips_through_upsert() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        assert_eq!(read_cursor(&read, "sess-1").unwrap(), None);
        drop(read);

        db.writer()
            .transaction(|tx| upsert_cursor(tx, "sess-1", 1, 100, 5000))
            .await
            .unwrap();
        let read = db.open_read().expect("read conn");
        assert_eq!(read_cursor(&read, "sess-1").unwrap(), Some((1, 100)));
        drop(read);

        db.writer()
            .transaction(|tx| upsert_cursor(tx, "sess-1", 2, 16, 6000))
            .await
            .unwrap();
        let read = db.open_read().expect("read conn");
        assert_eq!(read_cursor(&read, "sess-1").unwrap(), Some((2, 16)));
    }

    /// Isolates the **time** side of the window (spec 07 §5 "10 min"): a
    /// candidate that is not among the last `window_envelopes` (so the count
    /// clause is false) is still caught if its `source_timestamp` is within
    /// the floor.
    #[tokio::test]
    async fn window_dedup_time_boundary_is_inclusive_at_the_floor_and_exclusive_just_below() {
        let (_home, db) = open_state();
        let source_event_id = "up:sess-1:abc:100";
        db.writer()
            .transaction(move |tx| {
                insert_envelope(
                    tx,
                    &row("obs-1", "sess-1", source_event_id, None, Some(1_000)),
                )?;
                // A later, unrelated envelope so `obs-1` is no longer "the last 1".
                insert_envelope(tx, &row("obs-2", "sess-1", "unrelated", None, Some(2_000)))?;
                Ok(())
            })
            .await
            .unwrap();

        let read = db.open_read().expect("read conn");
        // window_envelopes = 1 => only the single most-recent row (`obs-2`)
        // satisfies the count clause; `obs-1` can only match via time.
        assert!(
            recent_same_source_event_exists(&read, "sess-1", source_event_id, 1_000, 1).unwrap(),
            "source_timestamp 1000 >= floor 1000: inclusive at the floor",
        );
        assert!(
            !recent_same_source_event_exists(&read, "sess-1", source_event_id, 1_001, 1).unwrap(),
            "source_timestamp 1000 < floor 1001: just outside the window",
        );
    }

    /// Isolates the **count** side of the window (spec 07 §5 "512 envelopes"):
    /// a candidate whose `source_timestamp` can never satisfy the time clause
    /// (`window_floor_ms = i64::MAX`) is still caught if it is among the last
    /// `window_envelopes` rows of the session.
    #[tokio::test]
    async fn window_dedup_count_boundary_is_inclusive_at_512_and_exclusive_at_513() {
        let (_home, db) = open_state();
        let source_event_id = "up:sess-1:abc:100";
        db.writer()
            .transaction(move |tx| {
                insert_envelope(
                    tx,
                    &row("obs-orig", "sess-1", source_event_id, None, Some(0)),
                )?;
                for i in 0..511 {
                    let id = format!("obs-pad-{i}");
                    let other = format!("other-{i}");
                    insert_envelope(tx, &row(&id, "sess-1", &other, None, Some(0)))?;
                }
                Ok(())
            })
            .await
            .unwrap();

        let read = db.open_read().expect("read conn");
        assert!(
            recent_same_source_event_exists(&read, "sess-1", source_event_id, i64::MAX, 512)
                .unwrap(),
            "obs-orig is exactly the 512th most recent envelope of the session",
        );
        drop(read);

        // One more pad row pushes `obs-orig` to 513th-most-recent.
        db.writer()
            .transaction(|tx| {
                insert_envelope(
                    tx,
                    &row("obs-pad-511", "sess-1", "other-511", None, Some(0)),
                )
            })
            .await
            .unwrap();
        let read = db.open_read().expect("read conn");
        assert!(
            !recent_same_source_event_exists(&read, "sess-1", source_event_id, i64::MAX, 512)
                .unwrap(),
            "obs-orig now falls outside the last-512 window (and the time clause never fires)",
        );
    }

    #[tokio::test]
    async fn max_received_seq_is_none_for_an_unknown_session_then_tracks_inserts() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        assert_eq!(max_received_seq(&read, "sess-1").unwrap(), None);
        drop(read);

        db.writer()
            .transaction(|tx| {
                insert_envelope(tx, &row("obs-1", "sess-1", "evt-1", None, Some(1)))?;
                insert_envelope(tx, &row("obs-2", "sess-1", "evt-2", None, Some(2)))?;
                // A different session must not influence sess-1's max.
                insert_envelope(tx, &row("obs-3", "sess-2", "evt-3", None, Some(3)))
            })
            .await
            .unwrap();

        let read = db.open_read().expect("read conn");
        assert_eq!(max_received_seq(&read, "sess-1").unwrap(), Some(2));
        assert_eq!(max_received_seq(&read, "sess-2").unwrap(), Some(3));
    }

    #[tokio::test]
    async fn envelopes_in_range_is_inclusive_both_ends_ordered_and_session_scoped() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| {
                for (i, sess) in [(1, "sess-1"), (2, "sess-1"), (3, "sess-1"), (4, "sess-1")] {
                    let id = format!("obs-{i}");
                    let evt = format!("evt-{i}");
                    insert_envelope(tx, &row(&id, sess, &evt, None, Some(i)))?;
                }
                // Out-of-session row at the same received_seq range must never surface.
                insert_envelope(tx, &row("obs-other", "sess-2", "evt-other", None, Some(5)))
            })
            .await
            .unwrap();

        let read = db.open_read().expect("read conn");
        let window = envelopes_in_range(&read, "sess-1", 2, 3).expect("window read");
        assert_eq!(
            window
                .iter()
                .map(|r| r.observation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["obs-2", "obs-3"],
            "boundary-inclusive at both ends, ascending by received_seq",
        );
        for r in &window {
            assert_eq!(r.evidence_kind, EvidenceKind::ModelClaim);
            assert_eq!(r.trust, TrustLevel::Low);
            assert_eq!(r.event_type, "Stop");
            assert_eq!(
                r.payload, None,
                "no observation_payload row was ever inserted"
            );
        }
    }

    #[tokio::test]
    async fn envelopes_in_range_reports_a_live_payload_and_none_once_it_is_absent() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| {
                insert_envelope(tx, &row("obs-with", "sess-1", "evt-1", None, Some(1)))?;
                insert_envelope(tx, &row("obs-without", "sess-1", "evt-2", None, Some(2)))?;
                insert_payload(tx, "obs-with", b"{\"x\":1}", 5000)
            })
            .await
            .unwrap();

        let read = db.open_read().expect("read conn");
        let window = envelopes_in_range(&read, "sess-1", 1, 2).expect("window read");
        assert_eq!(window.len(), 2);
        assert_eq!(
            window[0].payload.as_deref(),
            Some(&b"{\"x\":1}"[..]),
            "obs-with still has a live observation_payload row"
        );
        assert_eq!(
            window[1].payload, None,
            "obs-without never had a payload row (envelope-only event) — not an error"
        );
    }
}
