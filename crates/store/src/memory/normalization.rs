//! English normalization of durable memory text (spec 03 §2.5, ADR-0010) —
//! T21-01.
//!
//! This module ships migration **version 14** ([`SCHEMA_V14`]) and the
//! read/write layer over `memory_text_normalization`: at most one row per
//! `memory_entry`, recording the English variant of that entry's text, which
//! text it was derived from, and — when there is no usable variant — why not.
//!
//! **It has no consumer yet, deliberately.** T21-02 introduces the
//! `EffectiveText` type that decides which text is embedded, T21-03/T21-04 the
//! script detector and the translator, T21-05/T21-06 the write order and the
//! daemon worker. This task is the storage and its guards alone.
//!
//! ## Why a table of its own, not columns on `memory_entry`
//!
//! `memory_entry.text` is canonical and is never rewritten: spec 08 §3
//! `[FIXED]` lets only `edit` change it, and only together with a new
//! `entry_version` in the audit ledger, while `reinforce` may not touch it at
//! all — a background translator writing into that column would violate both.
//! The English variant is therefore a *derived* axis, and this project's
//! precedent for a derived axis is a separate table
//! ([`registry::indexing_status`](crate::registry) / X-006, migration 13),
//! not columns on the frozen original. A separate table also buys
//! `ON DELETE CASCADE` and a countable, explicit purge (T21-07) for free.
//!
//! ## Staleness is `canon_text_sha256`, never `entry_version`
//!
//! `apply_reinforce` bumps `entry_version` without touching the text
//! ([`super::op::apply_reinforce`]), so a version comparison would re-translate
//! entries whose text never changed and — worse — could be made to look current
//! by an unrelated bump. Every row therefore records the SHA-256 of the exact
//! text it was derived from, and a reader compares that against the entry's
//! text as it stands now.
//!
//! Every degraded case — no row, a hash that no longer matches, `skipped`,
//! `failed` — means "use the original text", which is the behaviour that
//! predates this table entirely and for which a vector already exists. The
//! migration is inert on upgrade for exactly that reason: an empty table is
//! indistinguishable from the pre-T21-01 store.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use local_rag_core::hash::sha256_hex;

/// Version-14 migration DDL: the English-normalization axis of durable memory
/// (spec 03 §2.5, ADR-0010).
///
/// A pure additive leaf, like [`SCHEMA_V13`](crate::registry): one table, one
/// outward foreign key, no backfill, no edit to any existing row. `ON DELETE
/// CASCADE` makes an entry's normalization disappear with the entry — the
/// property that lets T21-07's purge stay a single `DELETE` on
/// `memory_entry`.
///
/// The `CHECK ((status = 'ready') = (normalized_text IS NOT NULL))` is the
/// table's central invariant expressed where it cannot be bypassed: a `ready`
/// row always carries text a reader may use, and a `skipped`/`failed` row
/// never carries text a reader might mistake for a translation. Both halves
/// are enforced — a `ready` row without text and a `failed` row with one are
/// equally rejected.
///
/// The index is the queue's own access path
/// ([`entries_needing_normalization`]): `status` narrows to the retryable
/// rows, `next_attempt_at` orders the backoff gate.
///
/// **Frozen once shipped.** The migration checksum is the SHA-256 of this text
/// (see [`crate::migrate::Migration::checksum`]); any edit — even whitespace or
/// a comment — trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an
/// existing store. Future schema changes are new numbered migrations.
pub(crate) const SCHEMA_V14: &str = "\
CREATE TABLE memory_text_normalization (              -- migration 14; T21-01 (ADR-0010)
  memory_id           TEXT PRIMARY KEY REFERENCES memory_entry(memory_id) ON DELETE CASCADE,
  status              TEXT NOT NULL CHECK (status IN ('ready','skipped','failed')),
  source_text_sha256  TEXT NOT NULL,                  -- the text this row was derived from
  normalized_text     TEXT,                           -- NULL unless status='ready'
  source_language     TEXT,                           -- detector's answer, advisory
  normalizer_model_id TEXT,                           -- provenance: which model produced it
  prompt_version      INTEGER,                        -- provenance: which prompt
  normalizer_version  INTEGER NOT NULL,               -- bump re-normalizes every row
  attempt_count       INTEGER NOT NULL DEFAULT 0,
  last_error          TEXT,
  next_attempt_at     INTEGER,                        -- transient backoff gate, epoch ms
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  CHECK ((status = 'ready') = (normalized_text IS NOT NULL))
);
CREATE INDEX memory_normalization_queue
  ON memory_text_normalization(status, next_attempt_at);
-- memory_entry.text stays canonical and is never rewritten (08 §3 [FIXED]);
-- staleness is source_text_sha256, never entry_version (reinforce bumps the
-- version without touching the text).
";

/// Migration 15 (T21-13, ADR-0011): the same table, inverted.
///
/// English became the canon, so `memory_entry.text` now holds the English text
/// and this table holds what the **author** wrote. Every column that existed to
/// reconcile two texts is gone with the second text; what stays is the queue's
/// own bookkeeping, which ADR-0011 §Decision 3's "eventually English" invariant
/// still needs.
///
/// Column by column against [`SCHEMA_V14`]:
///
/// - `normalized_text` (the English variant) → `source_text` (the author's own
///   words). The CHECK mirrors: present exactly when `status = 'translated'`.
/// - `source_text_sha256` → `canon_text_sha256`. Same mechanism, inverted
///   subject: it is the hash of `memory_entry.text` **as this row last observed
///   it**, so the queue can tell a row that is still true from one whose entry
///   has since been edited. It is deliberately *not* dropped — see
///   [`NormalizationWrite`] for why the write-race guard it also serves splits
///   into two values rather than disappearing.
/// - `status`: `ready`/`skipped`/`failed` → `translated`/`english`/`failed`.
///   `english` is the old `skipped` and exists for one structural reason:
///   [`scan_pending`] is SQL with a `LIMIT`, and the detector cannot run inside
///   it. Without a stored "this canon is already English" marker the predicate
///   would return every English entry, fill the limit with them, and starve the
///   entries that actually need work.
///
/// **Data carried across, and the one loss, stated rather than implied.**
/// `skipped` rows become `english` (the entry is English and unchanged —
/// discarding them would make the detector re-examine them forever) and
/// `failed` rows keep their retry bookkeeping (T21-16 releases them by bumping
/// [`CURRENT_NORMALIZER_VERSION`]). `ready` rows are **dropped**: their English
/// text was never installed as canon and v15 has no state for "a translation
/// waiting to be installed". T21-16 replaces the translator that produced them
/// — the single-line JSON envelope that tore on long text — so preserving its
/// output would preserve the product of a component being declared defective.
/// T21-17 re-translates them.
///
/// **Not marked `destructive`, deliberately.** That flag's cost and meaning is a
/// `VACUUM INTO` copy of the whole `state.sqlite` before any mutation
/// (spec 13 §3, "rollback = restore + old binary") — on a real store that is a
/// multi-gigabyte copy, and here it would buy nothing. The one table rebuilt is
/// entirely **derived**: model output plus retry bookkeeping, reproducible by
/// re-running the translator. `memory_entry`, `memory_evidence` and
/// `audit_event` are not touched. The tripwire in
/// `crates/store/tests/migrate_fixtures.rs` therefore stays intact, and the
/// crash-at-checkpoint test it demands belongs to the first migration that is
/// destructive in the sense spec 13 §3 means.
///
/// **Frozen once shipped**, like every entry in [`crate::migrate::ALL`].
pub(crate) const SCHEMA_V15: &str = "CREATE TABLE memory_text_normalization_v15 (          -- migration 15; T21-13 (ADR-0011)
  memory_id           TEXT PRIMARY KEY REFERENCES memory_entry(memory_id) ON DELETE CASCADE,
  status              TEXT NOT NULL CHECK (status IN ('translated','english','failed')),
  canon_text_sha256   TEXT NOT NULL,                  -- memory_entry.text as this row saw it
  source_text         TEXT,                           -- the author's words; NULL unless 'translated'
  source_language     TEXT,                           -- detector's answer, advisory
  normalizer_model_id TEXT,                           -- provenance: which model produced the canon
  prompt_version      INTEGER,                        -- provenance: which prompt
  normalizer_version  INTEGER NOT NULL,               -- bump re-examines every row
  attempt_count       INTEGER NOT NULL DEFAULT 0,
  last_error          TEXT,
  next_attempt_at     INTEGER,                        -- transient backoff gate, epoch ms
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  CHECK ((status = 'translated') = (source_text IS NOT NULL))
);
INSERT INTO memory_text_normalization_v15
  (memory_id, status, canon_text_sha256, source_text, source_language,
   normalizer_model_id, prompt_version, normalizer_version, attempt_count,
   last_error, next_attempt_at, created_at, updated_at)
SELECT memory_id,
       CASE status WHEN 'skipped' THEN 'english' ELSE 'failed' END,
       source_text_sha256, NULL, source_language,
       normalizer_model_id, prompt_version, normalizer_version, attempt_count,
       last_error, next_attempt_at, created_at, updated_at
  FROM memory_text_normalization
 WHERE status IN ('skipped', 'failed');
DROP TABLE memory_text_normalization;
ALTER TABLE memory_text_normalization_v15 RENAME TO memory_text_normalization;
CREATE INDEX memory_normalization_queue
  ON memory_text_normalization(status, next_attempt_at);
-- memory_entry.text is the English canon (08 §3 [FIXED, ADR-0011]); this table
-- keeps the author's own words, and a text change deletes the row (T21-07).
";

/// The normalizer generation this binary produces (ADR-0010).
///
/// Bumping it re-normalizes every stored row: [`entries_needing_normalization`]
/// treats a lower `normalizer_version` as due, whatever its status — the same
/// "a new build earns one more attempt" shape D-050's build fingerprint already
/// established for consolidation, expressed as an explicit version rather than
/// a git hash because a normalizer change is a deliberate product decision, not
/// an incidental rebuild.
///
/// **Bumped 1 → 2 by `T21-16`**, which gave the translator grammar-constrained
/// decoding and raised its source limit. The bump is the mechanism, not a
/// courtesy: the dead-letter predicate is
/// `status='failed' AND attempt_count >= … AND normalizer_version >= <current>`,
/// so a row written under version 1 stops counting as a dead-letter the moment
/// current is 2 — and `entries_needing_normalization` offers it again. That is
/// what *releases* the entries an older normalizer gave up on. Translating them
/// is `T21-17`'s: nothing rewrites an existing entry's canon before that.
pub const CURRENT_NORMALIZER_VERSION: i64 = 2;

/// How many `failed` attempts one entry gets under a single normalizer version
/// before the queue stops offering it (ADR-0010 Decision 10: "failures degrade
/// to today's behaviour, never below it" — a dead-lettered entry simply keeps
/// using its original text).
///
/// Bumping [`CURRENT_NORMALIZER_VERSION`] releases it again, which is the only
/// escape hatch by design: retrying the same normalizer against the same text
/// is what D-050 proved to be an expensive way to reproduce a failure.
pub const MAX_NORMALIZATION_ATTEMPTS: i64 = 5;

/// Terminal `memory_entry.state` values, excluded from the normalization queue.
///
/// The same literal set [`recall_candidates_for_scope`](super::entry) filters
/// on: a resolved/retracted/rejected/superseded entry is never recalled, so
/// translating it would spend inference on text no reader will embed.
const TERMINAL_STATES: &str = "'resolved', 'retracted', 'rejected', 'superseded'";

/// What a normalization row says about its entry's canonical text.
///
/// Inverted by ADR-0011: the canon is English, so these describe **how the
/// entry's own text came to be English**, not which of two texts to prefer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizationStatus {
    /// The canon is a translation; `source_text` holds what the author wrote
    /// (non-`NULL`, enforced by the table's own CHECK).
    Translated,
    /// The canon was already English, so no inference was spent (ADR-0010
    /// Decision 8, still in force). There is no provenance to keep — the
    /// author's words *are* the canon.
    ///
    /// This state is not bookkeeping for its own sake: [`scan_pending`] is SQL
    /// with a `LIMIT` and cannot run the detector, so without a stored marker
    /// every English entry would be offered on every tick, fill the limit, and
    /// starve the entries that actually need work.
    English,
    /// Translation was attempted and produced nothing usable. The canon is
    /// still the author's text — ADR-0011 §Decision 3's "eventually English".
    Failed,
}

impl NormalizationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            NormalizationStatus::Translated => "translated",
            NormalizationStatus::English => "english",
            NormalizationStatus::Failed => "failed",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "translated" => Some(NormalizationStatus::Translated),
            "english" => Some(NormalizationStatus::English),
            "failed" => Some(NormalizationStatus::Failed),
            _ => None,
        }
    }

    /// Whether this state means the entry's canon needs no further work at the
    /// current normalizer version. `failed` does not — it is retried until the
    /// attempt ceiling.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            NormalizationStatus::Translated | NormalizationStatus::English
        )
    }
}

/// One stored `memory_text_normalization` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizationRow {
    pub memory_id: String,
    pub status: NormalizationStatus,
    /// SHA-256 of `memory_entry.text` **as this row last observed it** — the
    /// staleness basis, compared against the entry's current text.
    pub canon_text_sha256: String,
    /// What the author wrote, before the canon became English; `Some` exactly
    /// when `status == Translated`.
    pub source_text: Option<String>,
    /// The detector's answer for the source text, advisory only.
    pub source_language: Option<String>,
    pub normalizer_model_id: Option<String>,
    pub prompt_version: Option<i64>,
    pub normalizer_version: i64,
    pub attempt_count: i64,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One normalization outcome, as the caller already computed it.
///
/// Every field is supplied rather than derived in SQL — `attempt_count`
/// included, mirroring [`IndexingOutcome`](crate::registry::IndexingOutcome)'s
/// own reasoning: writing the computed value instead of an `x = x + 1`
/// increment is what makes [`upsert_normalization`] safe to replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizationWrite<'a> {
    pub memory_id: &'a str,
    pub status: NormalizationStatus,
    /// The **guard**: SHA-256 of the text the caller read before it started
    /// working. The write is refused unless `memory_entry.text` still hashes to
    /// this, so a translation of text an `edit` has since replaced can never be
    /// installed.
    ///
    /// This is separate from [`canon_text_sha256`](Self::canon_text_sha256)
    /// because installing a translation *changes* the canon in the same
    /// transaction: the guard describes what was there before, the stored hash
    /// what is there after. A writer that does not move the canon
    /// (`english`/`failed`) passes the same value for both.
    pub expected_text_sha256: &'a str,
    /// What to store as the row's staleness basis: SHA-256 of
    /// `memory_entry.text` as it stands once this write's transaction commits.
    pub canon_text_sha256: &'a str,
    /// The author's own text, kept as provenance. Required for
    /// [`NormalizationStatus::Translated`], forbidden otherwise — the table's
    /// CHECK rejects any other combination.
    pub source_text: Option<&'a str>,
    pub source_language: Option<&'a str>,
    pub normalizer_model_id: Option<&'a str>,
    pub prompt_version: Option<i64>,
    pub normalizer_version: i64,
    pub attempt_count: i64,
    pub last_error: Option<&'a str>,
    pub next_attempt_at: Option<i64>,
}

/// What [`upsert_normalization`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// The row was inserted or updated.
    Written,
    /// The entry's text no longer hashes to the write's `expected_text_sha256`:
    /// it changed while the caller was normalizing. **Nothing was written** —
    /// storing a translation of text that is no longer there would put the
    /// store in a state no reader can detect, since staleness is judged by
    /// exactly this hash.
    TextMoved {
        /// The entry's current hash, so a caller can re-queue without a second
        /// read.
        current_text_sha256: String,
    },
    /// No `memory_entry` with this id. Nothing was written.
    UnknownEntry,
}

/// Record one normalization outcome for `write.memory_id` (spec 03 §2.5).
///
/// The guard is the point of this function: it re-reads the entry's text **in
/// the caller's transaction**, hashes it, and refuses the write unless it still
/// matches `write.expected_text_sha256`. Normalization runs outside any
/// transaction and takes on the order of a second (ADR-0010 Decision 3), so an
/// `edit` landing in between is an ordinary race rather than an exotic one, and
/// the loser must be the stale translation.
///
/// On a match this is a full-row upsert, never a read-modify-write, so
/// replaying the same outcome — as a crash-retry may — leaves the row
/// identical instead of inflating `attempt_count`. `created_at` is preserved
/// across updates; `updated_at` always advances.
pub fn upsert_normalization(
    tx: &Transaction<'_>,
    write: &NormalizationWrite<'_>,
    now_ms: i64,
) -> rusqlite::Result<UpsertOutcome> {
    let current_text: Option<String> = tx
        .query_row(
            "SELECT text FROM memory_entry WHERE memory_id = ?1",
            params![write.memory_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(current_text) = current_text else {
        return Ok(UpsertOutcome::UnknownEntry);
    };
    let current_text_sha256 = sha256_hex(current_text.as_bytes());
    if current_text_sha256 != write.expected_text_sha256 {
        return Ok(UpsertOutcome::TextMoved {
            current_text_sha256,
        });
    }

    tx.execute(
        "INSERT INTO memory_text_normalization \
           (memory_id, status, canon_text_sha256, source_text, source_language, \
            normalizer_model_id, prompt_version, normalizer_version, attempt_count, \
            last_error, next_attempt_at, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12) \
         ON CONFLICT(memory_id) DO UPDATE SET \
           status              = ?2, \
           canon_text_sha256   = ?3, \
           source_text         = ?4, \
           source_language     = ?5, \
           normalizer_model_id = ?6, \
           prompt_version      = ?7, \
           normalizer_version  = ?8, \
           attempt_count       = ?9, \
           last_error          = ?10, \
           next_attempt_at     = ?11, \
           updated_at          = ?12",
        params![
            write.memory_id,
            write.status.as_str(),
            write.canon_text_sha256,
            write.source_text,
            write.source_language,
            write.normalizer_model_id,
            write.prompt_version,
            write.normalizer_version,
            write.attempt_count,
            write.last_error,
            write.next_attempt_at,
            now_ms,
        ],
    )?;
    Ok(UpsertOutcome::Written)
}

/// One entry's normalization row, or `None` if it has never been normalized.
pub fn normalization_for(
    conn: &Connection,
    memory_id: &str,
) -> rusqlite::Result<Option<NormalizationRow>> {
    conn.query_row(
        "SELECT memory_id, status, canon_text_sha256, source_text, source_language, \
                normalizer_model_id, prompt_version, normalizer_version, attempt_count, \
                last_error, next_attempt_at, created_at, updated_at \
         FROM memory_text_normalization WHERE memory_id = ?1",
        params![memory_id],
        row_to_normalization,
    )
    .optional()
}

/// One entry the normalization queue is offering, with the text to normalize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNormalization {
    pub memory_id: String,
    /// The entry's current text — the exact bytes whose SHA-256 the caller
    /// must pass back as `expected_text_sha256`.
    pub text: String,
    /// Attempts already recorded under this entry's existing row (`0` when
    /// there is none).
    pub attempt_count: i64,
}

/// Every non-terminal entry whose English variant is missing, stale, or due for
/// a retry — oldest first, capped at `limit` (spec 03 §2.5, ADR-0010).
///
/// Due means any of:
///
/// - no normalization row at all;
/// - `normalizer_version` below `normalizer_version` (a normalizer change
///   re-normalizes everything, whatever the previous status was);
/// - `failed`, under [`MAX_NORMALIZATION_ATTEMPTS`], and past `next_attempt_at`;
/// - a settled row (`translated`/`english`) whose `canon_text_sha256` no longer matches the entry's
///   text.
///
/// Terminal entries are excluded ([`TERMINAL_STATES`]) — recall never returns
/// them, so normalizing them would buy nothing.
///
/// The staleness comparison happens in Rust, not SQL: SQLite has no `sha256()`,
/// and registering a UDF for one was rejected in ADR-0010 precisely because it
/// would add a third definition of "the effective text" instead of removing the
/// second. Consequently `limit` is applied **after** that filter, so the scan
/// covers every non-terminal entry with a `ready`/`skipped` row — bounded by
/// the memory-entry count, which spec 08 §6 already caps for recall itself.
pub fn entries_needing_normalization(
    conn: &Connection,
    normalizer_version: i64,
    now_ms: i64,
    limit: usize,
) -> rusqlite::Result<Vec<PendingNormalization>> {
    scan_pending(conn, normalizer_version, now_ms, limit)
}

/// The one implementation of "which entries lag": both
/// [`entries_needing_normalization`] (the worker's queue) and
/// [`normalization_backlog`] (the `stats`/`doctor` number) go through it, so
/// the count a user reads and the work the daemon does can never describe
/// different sets.
fn scan_pending(
    conn: &Connection,
    normalizer_version: i64,
    now_ms: i64,
    limit: usize,
) -> rusqlite::Result<Vec<PendingNormalization>> {
    let sql = format!(
        "SELECT e.memory_id, e.text, COALESCE(n.attempt_count, 0), n.status, \
                n.canon_text_sha256, n.normalizer_version \
         FROM memory_entry e \
         LEFT JOIN memory_text_normalization n ON n.memory_id = e.memory_id \
         WHERE e.state NOT IN ({TERMINAL_STATES}) \
           AND ( \
                 n.memory_id IS NULL \
                 OR n.normalizer_version < ?1 \
                 OR ( \
                      n.status = 'failed' \
                      AND n.attempt_count < ?2 \
                      AND (n.next_attempt_at IS NULL OR n.next_attempt_at <= ?3) \
                    ) \
                 OR n.status IN ('translated', 'english') \
               ) \
         ORDER BY e.created_at, e.memory_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![normalizer_version, MAX_NORMALIZATION_ATTEMPTS, now_ms],
        |r| {
            let memory_id: String = r.get(0)?;
            let text: String = r.get(1)?;
            let attempt_count: i64 = r.get(2)?;
            let status: Option<String> = r.get(3)?;
            let canon_text_sha256: Option<String> = r.get(4)?;
            let row_version: Option<i64> = r.get(5)?;
            Ok((
                PendingNormalization {
                    memory_id,
                    text,
                    attempt_count,
                },
                status,
                canon_text_sha256,
                row_version,
            ))
        },
    )?;

    let mut out = Vec::new();
    for row in rows {
        let (pending, status, canon_text_sha256, row_version) = row?;
        // A row that SQL could not judge alone: a settled row at the current
        // normalizer version is due only if the canon moved under it.
        let settled = status
            .as_deref()
            .and_then(NormalizationStatus::from_db)
            .is_some_and(NormalizationStatus::is_settled)
            && row_version.is_some_and(|v| v >= normalizer_version);
        if settled
            && canon_text_sha256.as_deref() == Some(sha256_hex(pending.text.as_bytes()).as_str())
        {
            continue;
        }
        out.push(pending);
        if out.len() == limit {
            break;
        }
    }
    Ok(out)
}

/// One `(status, count)` bucket of [`normalization_counts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizationCountRow {
    pub status: NormalizationStatus,
    pub count: i64,
}

/// Every `memory_text_normalization` row grouped by status — store-wide, the
/// normalization twin of
/// [`consolidation_run_counts`](super::stats::consolidation_run_counts).
/// T21-08's `stats`/`doctor` surfaces read it; `GROUP BY` omits empty buckets
/// rather than reporting them as zero.
pub fn normalization_counts(conn: &Connection) -> rusqlite::Result<Vec<NormalizationCountRow>> {
    let mut stmt = conn.prepare(
        "SELECT status, COUNT(*) FROM memory_text_normalization GROUP BY status ORDER BY status",
    )?;
    let rows = stmt.query_map([], |r| {
        let raw: String = r.get(0)?;
        let status = NormalizationStatus::from_db(&raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("invalid memory_text_normalization.status {raw:?}").into(),
            )
        })?;
        Ok(NormalizationCountRow {
            status,
            count: r.get(1)?,
        })
    })?;
    rows.collect()
}

/// How much normalization work is outstanding, and how much has been given up
/// on ([`NormalizationBacklog`]).
///
/// `pending` runs the worker's own queue predicate ([`scan_pending`]) with no
/// limit rather than re-deriving it: a `stats` number that disagreed with what
/// the daemon actually picks up would be worse than no number. The scan is
/// bounded by the memory-entry count — the same bound spec 08 §6 already
/// accepts for recall — and reads each entry's text because the staleness
/// comparison is a SHA-256 in Rust (SQLite has no `sha256()`, and ADR-0010
/// rejected a UDF precisely to avoid a second definition of the effective
/// text).
///
/// `dead_letter` is pure SQL: rows the queue will never offer again under this
/// normalizer — `failed` at the attempt ceiling, at or past the current
/// version. Changing `CURRENT_NORMALIZER_VERSION` re-queues them, which is
/// exactly the "a new normalizer is a decision" rule T21-06's dead-letter is
/// keyed on.
pub fn normalization_backlog(
    conn: &Connection,
    normalizer_version: i64,
    now_ms: i64,
) -> rusqlite::Result<NormalizationBacklog> {
    let pending = scan_pending(conn, normalizer_version, now_ms, usize::MAX)?.len() as i64;
    let dead_letter: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_text_normalization \
         WHERE status = 'failed' AND attempt_count >= ?1 AND normalizer_version >= ?2",
        params![MAX_NORMALIZATION_ATTEMPTS, normalizer_version],
        |r| r.get(0),
    )?;
    Ok(NormalizationBacklog {
        pending,
        dead_letter,
    })
}

/// What [`normalization_backlog`] answers: work outstanding, and work
/// abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NormalizationBacklog {
    /// Non-terminal entries the worker's queue would offer right now — never
    /// normalized, normalized by an older normalizer, edited since, or failed
    /// and due for another attempt.
    pub pending: i64,
    /// `failed` rows at the attempt ceiling under the current normalizer: no
    /// tick will pick them up again until the normalizer version changes.
    pub dead_letter: i64,
}

/// The entries [`normalization_backlog`] counts as `dead_letter`, named — for
/// `doctor`, which has to answer *why* work stopped, not just how much of it
/// did (T21-08, the normalization twin of
/// [`stuck_consolidation_runs`](super::stats::stuck_consolidation_runs)).
///
/// `limit` bounds the report, not the count: a store with hundreds of
/// dead-lettered entries should print a readable list and let the number above
/// it carry the scale. `last_error` is truncated to
/// [`STUCK_RUN_REASON_MAX_CHARS`](super::stats::STUCK_RUN_REASON_MAX_CHARS) for
/// the same reason that constant exists — these rows exist to be printed, and a
/// model's refusal can be arbitrarily long.
pub fn dead_lettered_normalizations(
    conn: &Connection,
    normalizer_version: i64,
    limit: usize,
) -> rusqlite::Result<Vec<DeadLetteredNormalization>> {
    let mut stmt = conn.prepare(
        "SELECT memory_id, attempt_count, last_error, updated_at \
         FROM memory_text_normalization \
         WHERE status = 'failed' AND attempt_count >= ?1 AND normalizer_version >= ?2 \
         ORDER BY updated_at DESC, memory_id \
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![MAX_NORMALIZATION_ATTEMPTS, normalizer_version, limit as i64],
        |r| {
            Ok(DeadLetteredNormalization {
                memory_id: r.get(0)?,
                attempt_count: r.get(1)?,
                last_error: truncate_error(r.get(2)?),
                updated_at: r.get(3)?,
            })
        },
    )?;
    rows.collect()
}

/// One entry the normalizer has given up on under the current normalizer
/// version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetteredNormalization {
    pub memory_id: String,
    pub attempt_count: i64,
    /// Already truncated — see [`dead_lettered_normalizations`].
    pub last_error: Option<String>,
    pub updated_at: i64,
}

fn truncate_error(reason: Option<String>) -> Option<String> {
    reason.map(|r| {
        if r.chars().count() <= super::stats::STUCK_RUN_REASON_MAX_CHARS {
            return r;
        }
        let mut out: String = r
            .chars()
            .take(super::stats::STUCK_RUN_REASON_MAX_CHARS)
            .collect();
        out.push('…');
        out
    })
}

/// Drop one entry's normalization, returning whether a row was there.
///
/// Deleting the entry itself already cascades; this is the narrower operation
/// T21-07's privacy surfaces need — "forget the translation, keep the note" —
/// and the one a caller uses to force a re-normalization from scratch.
pub fn delete_normalization(tx: &Transaction<'_>, memory_id: &str) -> rusqlite::Result<bool> {
    let removed = tx.execute(
        "DELETE FROM memory_text_normalization WHERE memory_id = ?1",
        params![memory_id],
    )?;
    Ok(removed > 0)
}

fn row_to_normalization(r: &rusqlite::Row<'_>) -> rusqlite::Result<NormalizationRow> {
    let raw_status: String = r.get(1)?;
    let status = NormalizationStatus::from_db(&raw_status).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("invalid memory_text_normalization.status {raw_status:?}").into(),
        )
    })?;
    Ok(NormalizationRow {
        memory_id: r.get(0)?,
        status,
        canon_text_sha256: r.get(2)?,
        source_text: r.get(3)?,
        source_language: r.get(4)?,
        normalizer_model_id: r.get(5)?,
        prompt_version: r.get(6)?,
        normalizer_version: r.get(7)?,
        attempt_count: r.get(8)?,
        last_error: r.get(9)?,
        next_attempt_at: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare `memory_entry` foreign-key parent plus this module's own table —
    /// the minimum `SCHEMA_V14` needs — with foreign keys ON, as `state::open`
    /// sets them on every real `state.sqlite` connection.
    fn conn_with_normalization() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn.execute_batch(
            "CREATE TABLE memory_entry (\
               memory_id TEXT PRIMARY KEY, \
               state TEXT NOT NULL, \
               text TEXT NOT NULL, \
               created_at INTEGER NOT NULL);",
        )
        .unwrap();
        // v14 **then** v15: the migration is the only definition of the
        // current shape, and building the table any other way would let these
        // tests pass against a schema the store never actually has.
        conn.execute_batch(SCHEMA_V14).unwrap();
        conn.execute_batch(SCHEMA_V15).unwrap();
        conn
    }

    fn seed_entry(conn: &Connection, memory_id: &str, state: &str, text: &str, created_at: i64) {
        conn.execute(
            "INSERT INTO memory_entry (memory_id, state, text, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![memory_id, state, text, created_at],
        )
        .unwrap();
    }

    /// `source_sha` is the caller's own `sha256_hex` of the text being
    /// normalized, held by the caller so the write borrows it.
    fn ready<'a>(
        memory_id: &'a str,
        source_sha: &'a str,
        english: &'a str,
    ) -> NormalizationWrite<'a> {
        NormalizationWrite {
            memory_id,
            status: NormalizationStatus::Translated,
            expected_text_sha256: source_sha,
            canon_text_sha256: source_sha,
            source_text: Some(english),
            source_language: Some("ru"),
            normalizer_model_id: Some("gemma-4-e2b-it-gguf-q4-0"),
            prompt_version: Some(1),
            normalizer_version: CURRENT_NORMALIZER_VERSION,
            attempt_count: 1,
            last_error: None,
            next_attempt_at: None,
        }
    }

    fn upsert(conn: &mut Connection, write: &NormalizationWrite<'_>, now: i64) -> UpsertOutcome {
        let tx = conn.transaction().unwrap();
        let outcome = upsert_normalization(&tx, write, now).unwrap();
        tx.commit().unwrap();
        outcome
    }

    #[test]
    fn status_round_trips_and_rejects_an_unknown_value() {
        for status in [
            NormalizationStatus::Translated,
            NormalizationStatus::English,
            NormalizationStatus::Failed,
        ] {
            assert_eq!(NormalizationStatus::from_db(status.as_str()), Some(status));
        }
        // The pre-v15 vocabulary is not merely absent, it is refused: a store
        // that somehow still holds `ready` must read as corrupt, not as
        // translated.
        for retired in ["ready", "skipped", "", "READY"] {
            assert_eq!(NormalizationStatus::from_db(retired), None, "{retired:?}");
        }
    }

    /// The table's central invariant, both directions: `translated` always
    /// carries the author's own text, and nothing else ever carries text a
    /// reader might mistake for provenance.
    #[test]
    fn the_check_binds_translated_to_text_in_both_directions() {
        let conn = conn_with_normalization();
        seed_entry(&conn, "m-1", "active", "исходный текст", 1_000);

        let translated_without_text = conn.execute(
            "INSERT INTO memory_text_normalization \
               (memory_id, status, canon_text_sha256, source_text, normalizer_version, \
                created_at, updated_at) \
             VALUES ('m-1', 'translated', 'abc', NULL, 1, 1000, 1000)",
            [],
        );
        assert_eq!(
            translated_without_text.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
            "a translated row with no source_text would lose the words it exists to keep"
        );

        let failed_with_text = conn.execute(
            "INSERT INTO memory_text_normalization \
               (memory_id, status, canon_text_sha256, source_text, normalizer_version, \
                created_at, updated_at) \
             VALUES ('m-1', 'failed', 'abc', 'some english', 1, 1000, 1000)",
            [],
        );
        assert_eq!(
            failed_with_text.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
            "source_text on a non-translated row is provenance for a translation that \
             never happened"
        );

        // A status outside the domain — including the pre-v15 vocabulary,
        // which is exactly what a half-finished rename would leave behind.
        let unknown_status = conn.execute(
            "INSERT INTO memory_text_normalization \
               (memory_id, status, canon_text_sha256, source_text, normalizer_version, \
                created_at, updated_at) \
             VALUES ('m-1', 'ready', 'abc', 'the author''s words', 1, 1000, 1000)",
            [],
        );
        assert_eq!(
            unknown_status.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
        );
    }

    #[test]
    fn an_unknown_memory_id_is_rejected_by_the_foreign_key() {
        let conn = conn_with_normalization();
        let err = conn
            .execute(
                "INSERT INTO memory_text_normalization \
                   (memory_id, status, canon_text_sha256, source_text, normalizer_version, \
                    created_at, updated_at) \
                 VALUES ('ghost', 'ready', 'abc', 'english', 1, 1000, 1000)",
                [],
            )
            .expect_err("a normalization row must never dangle");
        assert_eq!(
            err.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
        );
    }

    #[test]
    fn deleting_the_entry_cascades_its_normalization_away() {
        let mut conn = conn_with_normalization();
        seed_entry(&conn, "m-1", "active", "исходный текст", 1_000);
        let sha = sha256_hex("исходный текст".as_bytes());
        upsert(&mut conn, &ready("m-1", &sha, "source text"), 2_000);
        assert!(normalization_for(&conn, "m-1").unwrap().is_some());

        conn.execute("DELETE FROM memory_entry WHERE memory_id = 'm-1'", [])
            .unwrap();
        assert_eq!(
            normalization_for(&conn, "m-1").unwrap(),
            None,
            "ON DELETE CASCADE is what makes T21-07's purge a single DELETE"
        );
    }

    #[test]
    fn upsert_is_idempotent_and_keeps_created_at() {
        let mut conn = conn_with_normalization();
        seed_entry(&conn, "m-1", "active", "исходный текст", 1_000);
        let sha = sha256_hex("исходный текст".as_bytes());
        let write = ready("m-1", &sha, "source text");

        assert_eq!(upsert(&mut conn, &write, 2_000), UpsertOutcome::Written);
        let first = normalization_for(&conn, "m-1").unwrap().unwrap();
        assert_eq!(upsert(&mut conn, &write, 3_000), UpsertOutcome::Written);
        let second = normalization_for(&conn, "m-1").unwrap().unwrap();

        assert_eq!(
            second.created_at, first.created_at,
            "a replay must not restart the row's history"
        );
        assert_eq!(second.updated_at, 3_000);
        assert_eq!(second.attempt_count, first.attempt_count, "no x = x + 1");
        assert_eq!(second.source_text.as_deref(), Some("source text"));
        assert_eq!(second.status, NormalizationStatus::Translated);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM memory_text_normalization", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            1,
        );
    }

    /// The guard this write exists for: normalization runs outside any
    /// transaction, so an `edit` can land while it works. The stale
    /// translation must lose, and lose without writing anything.
    #[test]
    fn upsert_refuses_a_translation_of_text_that_has_since_changed() {
        let mut conn = conn_with_normalization();
        seed_entry(&conn, "m-1", "active", "исходный текст", 1_000);
        let sha = sha256_hex("исходный текст".as_bytes());
        let write = ready("m-1", &sha, "source text");

        conn.execute(
            "UPDATE memory_entry SET text = 'совершенно другой текст' WHERE memory_id = 'm-1'",
            [],
        )
        .unwrap();

        let outcome = upsert(&mut conn, &write, 2_000);
        assert_eq!(
            outcome,
            UpsertOutcome::TextMoved {
                current_text_sha256: sha256_hex("совершенно другой текст".as_bytes()),
            },
        );
        assert_eq!(
            normalization_for(&conn, "m-1").unwrap(),
            None,
            "a refused write must leave no row at all"
        );
    }

    #[test]
    fn upsert_reports_an_unknown_entry_without_writing() {
        let mut conn = conn_with_normalization();
        let sha = sha256_hex("исходный текст".as_bytes());
        let write = ready("ghost", &sha, "source text");
        assert_eq!(
            upsert(&mut conn, &write, 2_000),
            UpsertOutcome::UnknownEntry
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM memory_text_normalization", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            0,
        );
    }

    #[test]
    fn counts_group_by_status_and_omit_empty_buckets() {
        let mut conn = conn_with_normalization();
        seed_entry(&conn, "m-1", "active", "один", 1_000);
        seed_entry(&conn, "m-2", "active", "two", 1_001);
        let sha = sha256_hex("один".as_bytes());
        upsert(&mut conn, &ready("m-1", &sha, "one"), 2_000);
        upsert(
            &mut conn,
            &NormalizationWrite {
                memory_id: "m-2",
                status: NormalizationStatus::English,
                expected_text_sha256: &sha256_hex(b"two"),
                canon_text_sha256: &sha256_hex(b"two"),
                source_text: None,
                source_language: Some("en"),
                normalizer_model_id: None,
                prompt_version: None,
                normalizer_version: CURRENT_NORMALIZER_VERSION,
                attempt_count: 0,
                last_error: None,
                next_attempt_at: None,
            },
            2_000,
        );

        // `ORDER BY status` is the reader's contract, so the expected order is
        // alphabetical on the stored value: english < translated.
        assert_eq!(
            normalization_counts(&conn).unwrap(),
            vec![
                NormalizationCountRow {
                    status: NormalizationStatus::English,
                    count: 1,
                },
                NormalizationCountRow {
                    status: NormalizationStatus::Translated,
                    count: 1,
                },
            ],
            "no 'failed' bucket is invented for a store that has none"
        );
    }

    #[test]
    fn delete_removes_the_variant_and_reports_whether_one_was_there() {
        let mut conn = conn_with_normalization();
        seed_entry(&conn, "m-1", "active", "исходный текст", 1_000);
        let sha = sha256_hex("исходный текст".as_bytes());
        upsert(&mut conn, &ready("m-1", &sha, "source text"), 2_000);

        let tx = conn.transaction().unwrap();
        assert!(delete_normalization(&tx, "m-1").unwrap());
        assert!(!delete_normalization(&tx, "m-1").unwrap());
        tx.commit().unwrap();
        assert_eq!(normalization_for(&conn, "m-1").unwrap(), None);
    }

    // ---- T21-08: the numbers `stats`/`doctor` report -----------------------

    #[test]
    fn the_backlog_counts_exactly_what_the_queue_would_offer() {
        let mut conn = conn_with_normalization();
        seed_entry(&conn, "m-fresh", "active", "никогда не переводилась", 1);
        seed_entry(&conn, "m-done", "active", "already english", 2);
        seed_entry(&conn, "m-moved", "active", "текст уехал после перевода", 3);
        seed_entry(&conn, "m-terminal", "retracted", "отозванная запись", 4);

        upsert(
            &mut conn,
            &ready("m-done", &sha256_hex(b"already english"), "already english"),
            100,
        );
        // Written against the text as it was, then the entry was edited: the
        // row is stale and the entry is due again.
        upsert(
            &mut conn,
            &ready(
                "m-moved",
                &sha256_hex("текст уехал после перевода".as_bytes()),
                "the text moved after translation",
            ),
            100,
        );
        conn.execute(
            "UPDATE memory_entry SET text = ?2 WHERE memory_id = ?1",
            params!["m-moved", "текст уже другой"],
        )
        .unwrap();

        let backlog = normalization_backlog(&conn, CURRENT_NORMALIZER_VERSION, 200).unwrap();
        assert_eq!(
            backlog.pending, 2,
            "m-fresh and m-moved; the terminal entry is never offered",
        );
        assert_eq!(backlog.dead_letter, 0);

        // The count and the queue describe the same set, by construction.
        let queued =
            entries_needing_normalization(&conn, CURRENT_NORMALIZER_VERSION, 200, usize::MAX)
                .unwrap();
        assert_eq!(queued.len() as i64, backlog.pending);
    }

    #[test]
    fn a_row_at_the_attempt_ceiling_is_dead_letter_not_pending() {
        let mut conn = conn_with_normalization();
        seed_entry(&conn, "m-1", "active", "текст, который не поддался", 1);
        let sha = sha256_hex("текст, который не поддался".as_bytes());
        upsert(
            &mut conn,
            &NormalizationWrite {
                memory_id: "m-1",
                status: NormalizationStatus::Failed,
                expected_text_sha256: &sha,
                canon_text_sha256: &sha,
                source_text: None,
                source_language: Some("ru"),
                normalizer_model_id: Some("gemma-4-e2b-it-gguf-q4-0"),
                prompt_version: Some(1),
                normalizer_version: CURRENT_NORMALIZER_VERSION,
                attempt_count: MAX_NORMALIZATION_ATTEMPTS,
                last_error: Some("answer was not one {\"en\": …} object"),
                next_attempt_at: None,
            },
            100,
        );

        let backlog = normalization_backlog(&conn, CURRENT_NORMALIZER_VERSION, 200).unwrap();
        assert_eq!(backlog.dead_letter, 1);
        assert_eq!(
            backlog.pending, 0,
            "nothing will pick it up again under this normalizer",
        );

        // A new normalizer is a decision, and it re-queues the row.
        let next = normalization_backlog(&conn, CURRENT_NORMALIZER_VERSION + 1, 200).unwrap();
        assert_eq!(next.pending, 1);
        assert_eq!(
            next.dead_letter, 0,
            "the ceiling is per normalizer version, not forever",
        );
    }

    #[test]
    fn a_failed_row_still_within_its_backoff_is_neither_pending_nor_dead_letter() {
        let mut conn = conn_with_normalization();
        seed_entry(&conn, "m-1", "active", "текст на повторную попытку", 1);
        let sha = sha256_hex("текст на повторную попытку".as_bytes());
        upsert(
            &mut conn,
            &NormalizationWrite {
                memory_id: "m-1",
                status: NormalizationStatus::Failed,
                expected_text_sha256: &sha,
                canon_text_sha256: &sha,
                source_text: None,
                source_language: Some("ru"),
                normalizer_model_id: Some("gemma-4-e2b-it-gguf-q4-0"),
                prompt_version: Some(1),
                normalizer_version: CURRENT_NORMALIZER_VERSION,
                attempt_count: 1,
                last_error: Some("model busy"),
                next_attempt_at: Some(5_000),
            },
            100,
        );

        let waiting = normalization_backlog(&conn, CURRENT_NORMALIZER_VERSION, 1_000).unwrap();
        assert_eq!(waiting.pending, 0, "the backoff gate has not opened yet");
        assert_eq!(waiting.dead_letter, 0, "one attempt is not a dead letter");

        let due = normalization_backlog(&conn, CURRENT_NORMALIZER_VERSION, 5_000).unwrap();
        assert_eq!(due.pending, 1, "at the deadline it is due again");
    }

    #[test]
    fn an_empty_store_reports_zero_of_both() {
        let conn = conn_with_normalization();
        assert_eq!(
            normalization_backlog(&conn, CURRENT_NORMALIZER_VERSION, 1_000).unwrap(),
            NormalizationBacklog::default(),
        );
    }
}
