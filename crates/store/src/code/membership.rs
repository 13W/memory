//! Generation-membership, path-dependent code tables (spec 03 §2.4):
//! `generation_file`, `skipped_file`, `generation_unit_occurrence`,
//! `unresolved_reference`, `resolved_graph_edge`.
//!
//! These are the only legitimate home for a `normalized_path`/`display_path`
//! outside the two path ledgers (spec 01 §5.1). The strict source-blob invariant
//! (spec 12 §5) is structural here: a `generation_unit_occurrence` can only exist
//! on a `generation_file` member (composite FK `(generation_id, normalized_path)`),
//! and a member always resolves to a `file_revision` with a non-null
//! `source_blob`. A `skipped_file` therefore never gets an occurrence (spec 06
//! §2.2).
//!
//! [`insert_occurrence`] stores what it is given: the `occurrence_id` is a
//! caller-supplied string (T03-01 verbatim contract). Its deterministic
//! derivation [`occurrence_id`] — `H(occurrence_id: generation_id,
//! normalized_path, unit_id)` (spec 03 §1.2) — is T05-01; the generation *builder*
//! that mints and persists occurrences with it is T05-03. Write operations take a
//! [`Transaction`]; reads take a [`Connection`].

use local_rag_core::identity::Domain;
use local_rag_core::identity::domain::hash;
use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

use super::revision::UnitKind;

/// The deterministic `generation_unit_occurrence.occurrence_id` (spec 03 §1.2,
/// §2.4): `H(occurrence_id: generation_id, normalized_path, unit_id)` — group 05.
///
/// The domain (`local-rag/1/occurrence_id`) fixes the field order
/// `generation_id, normalized_path, unit_id` (spec 03 §1.2 table); all three are
/// text / already-hex identities, so each is hashed as its exact UTF-8/ASCII bytes
/// (the codebase's serialization convention). The id depends on nothing but its
/// own tuple, so it is stable under retry/reconcile and independent of row
/// insertion order (spec 03 §1.2 `[FIXED]`) — the property the generation builder
/// (T05-03) relies on to re-derive identical ids for unchanged occurrences.
///
/// This is the *derivation* only; [`insert_occurrence`] still stores the id
/// verbatim (its T03-01 contract). A caller binds the owned `String` and borrows
/// it into a [`NewOccurrence`].
pub fn occurrence_id(generation_id: &str, normalized_path: &str, unit_id: &str) -> String {
    hash(
        Domain::OccurrenceId,
        &[
            generation_id.as_bytes(),
            normalized_path.as_bytes(),
            unit_id.as_bytes(),
        ],
    )
}

/// Why a file was skipped from the searchable generation (spec 03 §2.4
/// `skipped_file.reason`, policy in spec 06 §2.2 / 12 §2, §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Binary content (NUL heuristic + extension list).
    Binary,
    /// A Git-LFS pointer file.
    Lfs,
    /// Larger than the configured `max_file_size_kb`.
    Huge,
    /// The redaction scanner flagged secrets (spec 12 §2).
    Secret,
    /// Excluded by gitignore or a configured exclude.
    Ignored,
    /// An unsupported encoding (no transcoding without an offset mapping).
    Encoding,
}

impl SkipReason {
    /// The stored `skipped_file.reason` value.
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::Binary => "binary",
            SkipReason::Lfs => "lfs",
            SkipReason::Huge => "huge",
            SkipReason::Secret => "secret",
            SkipReason::Ignored => "ignored",
            SkipReason::Encoding => "encoding",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "binary" => Some(SkipReason::Binary),
            "lfs" => Some(SkipReason::Lfs),
            "huge" => Some(SkipReason::Huge),
            "secret" => Some(SkipReason::Secret),
            "ignored" => Some(SkipReason::Ignored),
            "encoding" => Some(SkipReason::Encoding),
            _ => None,
        }
    }
}

/// How a resolved graph edge was derived (spec 03 §2.4
/// `resolved_graph_edge.resolution`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeResolution {
    /// A best-effort heuristic match.
    Heuristic,
    /// A syntax-level resolution.
    Syntax,
    /// An LSP-backed resolution (post-v0).
    Lsp,
}

impl EdgeResolution {
    /// The stored `resolved_graph_edge.resolution` value.
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeResolution::Heuristic => "heuristic",
            EdgeResolution::Syntax => "syntax",
            EdgeResolution::Lsp => "lsp",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "heuristic" => Some(EdgeResolution::Heuristic),
            "syntax" => Some(EdgeResolution::Syntax),
            "lsp" => Some(EdgeResolution::Lsp),
            _ => None,
        }
    }
}

/// Insert a `generation_file` membership row (spec 03 §2.4).
///
/// Binds `normalized_path` in `generation_id` to a shared `file_revision`.
/// `PRIMARY KEY (generation_id, normalized_path)` makes a path appear at most once
/// per generation; an unknown `generation_id`/`file_revision_id` is rejected by
/// the foreign keys.
pub fn insert_generation_file(
    tx: &Transaction<'_>,
    generation_id: &str,
    normalized_path: &str,
    display_path: &str,
    file_revision_id: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO generation_file \
           (generation_id, normalized_path, display_path, file_revision_id) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            generation_id,
            normalized_path,
            display_path,
            file_revision_id
        ],
    )?;
    Ok(())
}

/// Insert a `skipped_file` row (spec 03 §2.4).
///
/// A skipped file records `(path, reason, optional content_hash)` and — by the
/// absence of any occurrence — is absent from the searchable generation (spec 12
/// §5). `content_hash` is optional (e.g. an `ignored` file need not be hashed).
pub fn insert_skipped_file(
    tx: &Transaction<'_>,
    generation_id: &str,
    normalized_path: &str,
    reason: SkipReason,
    content_hash: Option<&str>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO skipped_file (generation_id, normalized_path, reason, content_hash) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            generation_id,
            normalized_path,
            reason.as_str(),
            content_hash
        ],
    )?;
    Ok(())
}

/// A row to insert into `generation_unit_occurrence` (spec 03 §2.4).
///
/// `occurrence_id` is the deterministic `H(occurrence_id, …)` (spec 03 §1.2) —
/// supplied by the generation builder (group 05); T03-01 stores it verbatim. The
/// `(generation_id, normalized_path)` pair MUST already be a `generation_file`
/// member (composite FK), which is how the strict source-blob invariant is
/// enforced structurally.
#[derive(Debug, Clone, Copy)]
pub struct NewOccurrence<'a> {
    /// Deterministic occurrence id.
    pub occurrence_id: &'a str,
    /// The generation this occurrence belongs to.
    pub generation_id: &'a str,
    /// The member path (must exist in `generation_file`).
    pub normalized_path: &'a str,
    /// The parsed unit this occurrence projects.
    pub unit_id: &'a str,
    /// Optional fully-qualified name.
    pub qualified_name: Option<&'a str>,
    /// Optional occurrence-context hash (path-dependent by definition).
    pub context_hash: Option<&'a str>,
}

/// Insert a `generation_unit_occurrence` row (spec 03 §2.4).
///
/// Fails with a [`ConstraintViolation`](rusqlite::ErrorCode::ConstraintViolation)
/// when `(generation_id, normalized_path)` is not a `generation_file` member (the
/// structural source-blob invariant), when `unit_id` is unknown, or on a duplicate
/// `(generation_id, normalized_path, unit_id)`.
pub fn insert_occurrence(tx: &Transaction<'_>, occ: &NewOccurrence<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO generation_unit_occurrence \
           (occurrence_id, generation_id, normalized_path, unit_id, qualified_name, context_hash) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            occ.occurrence_id,
            occ.generation_id,
            occ.normalized_path,
            occ.unit_id,
            occ.qualified_name,
            occ.context_hash,
        ],
    )?;
    Ok(())
}

/// A row to insert into `unresolved_reference` (spec 03 §2.4).
///
/// Parse-local, per file revision: a reference emitted while parsing `source_unit`
/// that could not be resolved within the file.
#[derive(Debug, Clone, Copy)]
pub struct NewUnresolvedReference<'a> {
    /// The revision the reference was parsed from.
    pub file_revision_id: &'a str,
    /// The unit that emitted the reference.
    pub source_unit_id: &'a str,
    /// The raw reference text.
    pub reference_text: &'a str,
    /// The reference kind (import/call/…).
    pub reference_kind: &'a str,
}

/// Insert an `unresolved_reference` row (spec 03 §2.4).
pub fn insert_unresolved_reference(
    tx: &Transaction<'_>,
    reference: &NewUnresolvedReference<'_>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO unresolved_reference \
           (file_revision_id, source_unit_id, reference_text, reference_kind) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            reference.file_revision_id,
            reference.source_unit_id,
            reference.reference_text,
            reference.reference_kind,
        ],
    )?;
    Ok(())
}

/// Delete every `unresolved_reference` row for a file revision, returning the
/// number removed (spec 03 §2.4).
///
/// `unresolved_reference` has no natural key (a file may legitimately repeat a
/// specifier, e.g. two `import "x"`), so idempotent re-persistence of a revision's
/// references is a scoped clear-then-reinsert rather than a per-row create-or-reuse.
/// The scan is served by the `unresolved_by_rev` index. Run inside the same
/// transaction as the reinsert so a retry converges on the identical row set.
pub fn delete_unresolved_references_for_revision(
    tx: &Transaction<'_>,
    file_revision_id: &str,
) -> rusqlite::Result<usize> {
    tx.execute(
        "DELETE FROM unresolved_reference WHERE file_revision_id = ?1",
        params![file_revision_id],
    )
}

/// A row to insert into `resolved_graph_edge` (spec 03 §2.4).
///
/// Per generation, on occurrence ids. `edge_kind` is an open-ended label
/// (`[OPEN]` final graph semantics); `resolution` is the CHECK-constrained
/// [`EdgeResolution`]. Post-v0 in the reconcile pipeline (spec 06 §2), but the
/// table and its typed insert ship with the schema.
#[derive(Debug, Clone, Copy)]
pub struct NewResolvedEdge<'a> {
    /// The generation this edge belongs to.
    pub generation_id: &'a str,
    /// Source occurrence.
    pub src_occurrence_id: &'a str,
    /// Destination occurrence.
    pub dst_occurrence_id: &'a str,
    /// Edge kind label (`import`/`call_heuristic`/…).
    pub edge_kind: &'a str,
    /// How the edge was resolved.
    pub resolution: EdgeResolution,
}

/// Insert a `resolved_graph_edge` row (spec 03 §2.4).
pub fn insert_resolved_edge(
    tx: &Transaction<'_>,
    edge: &NewResolvedEdge<'_>,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO resolved_graph_edge \
           (generation_id, src_occurrence_id, dst_occurrence_id, edge_kind, resolution) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            edge.generation_id,
            edge.src_occurrence_id,
            edge.dst_occurrence_id,
            edge.edge_kind,
            edge.resolution.as_str(),
        ],
    )?;
    Ok(())
}

/// The `file_revision_id` a `normalized_path` maps to in `generation_id`, if it
/// is a member (spec 03 §2.4).
pub fn member_file_revision(
    conn: &Connection,
    generation_id: &str,
    normalized_path: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT file_revision_id FROM generation_file \
         WHERE generation_id = ?1 AND normalized_path = ?2",
        params![generation_id, normalized_path],
        |r| r.get(0),
    )
    .optional()
}

/// Every `occurrence_id` recorded for `generation_id`, ascending (spec 03 §2.4).
///
/// Served by the `occurrence_by_gen` index. Ordering is only for tidy,
/// reproducible output — occurrence identity is order-independent (spec 03
/// §1.2 `[FIXED]`) and the projection switch (T07-03) re-sorts/de-duplicates
/// the point ids it derives from this list anyway.
pub fn occurrence_ids_for_generation(
    conn: &Connection,
    generation_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT occurrence_id FROM generation_unit_occurrence \
         WHERE generation_id = ?1 ORDER BY occurrence_id",
    )?;
    let ids = stmt
        .query_map(params![generation_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// Every `(occurrence_id, blob_id)` pair for `generation_id`, joined through
/// `parsed_unit` (spec 03 §2.4, T11-02). The content-shared identity a `code_raw`
/// embedding subject hashes against ([`local_rag_core::identity::domain::subject_content_blob`]) —
/// two occurrences with the same `blob_id` here resolve to the same subject hash
/// (structural sharing, spec 06 §2).
pub fn content_blob_ids_for_generation(
    conn: &Connection,
    generation_id: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT guo.occurrence_id, pu.blob_id \
         FROM generation_unit_occurrence guo \
         JOIN parsed_unit pu ON pu.unit_id = guo.unit_id \
         WHERE guo.generation_id = ?1 \
         ORDER BY guo.occurrence_id",
    )?;
    let rows = stmt
        .query_map(params![generation_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One occurrence's source data for FTS materialization (spec 06 §2/§4, T08-02):
/// everything the FTS materializer (`cache::fts::materialize_fts`) needs from
/// `state.sqlite` for one `generation_unit_occurrence` row, joined against its
/// `parsed_unit` and `content_blob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsSourceRow {
    /// The deterministic, generation-scoped occurrence id.
    pub occurrence_id: String,
    /// The member path this occurrence belongs to.
    pub normalized_path: String,
    /// Fully-qualified name, if derived (always `None` on real data today — no
    /// caller derives one yet, spec 06 §2 as-built note).
    pub qualified_name: Option<String>,
    /// The unit's kind (spec 03 §2.3). Carried for completeness/future use
    /// (e.g. verifying "all kinds indexed", spec 09 §1); `fts_doc`/
    /// `fts_occurrences` have no column for it.
    pub unit_kind: UnitKind,
    /// Optional local (unqualified) name — the `name` FTS column's source.
    pub local_name: Option<String>,
    /// The content blob whose normalized text is this occurrence's `body`.
    pub blob_id: String,
    /// The revision `blob_id` was derived from — needed only to recompute an
    /// evicted `normalized_text_cache` row (re-slice `source_blob`), never for
    /// the FTS columns themselves.
    pub file_revision_id: String,
    /// Byte offset of the unit's span start in `source_blob` (recompute-only).
    pub span_start: i64,
    /// Byte offset of the unit's span end in `source_blob` (recompute-only).
    pub span_end: i64,
    /// The content blob's language label.
    pub language: String,
}

/// Every occurrence of `generation_id`, joined against `parsed_unit`/`content_blob`
/// (spec 06 §2/§4, T08-02) — the source data the FTS materializer needs. Ordered
/// by `occurrence_id` for reproducible output (occurrence identity is itself
/// order-independent, spec 03 §1.2 `[FIXED]`).
///
/// The first multi-table join in this codebase: [`occurrence_ids_for_generation`]
/// and the dense projection's `expected_points` (`local-rag-projection`) both read
/// only bare occurrence ids, which is not enough to populate `fts_occurrences`'
/// `name`/`path`/`qualified_name`/`body` columns.
pub fn occurrences_for_fts(
    conn: &Connection,
    generation_id: &str,
) -> rusqlite::Result<Vec<FtsSourceRow>> {
    let mut stmt = conn.prepare(
        "SELECT o.occurrence_id, o.normalized_path, o.qualified_name, \
                pu.unit_kind, pu.local_name, pu.blob_id, \
                pu.file_revision_id, pu.span_start, pu.span_end, \
                cb.language \
         FROM generation_unit_occurrence o \
         JOIN parsed_unit pu ON pu.unit_id = o.unit_id \
         JOIN content_blob cb ON cb.blob_id = pu.blob_id \
         WHERE o.generation_id = ?1 \
         ORDER BY o.occurrence_id",
    )?;
    let rows = stmt
        .query_map(params![generation_id], |r| {
            let unit_kind_raw: String = r.get(3)?;
            let unit_kind = UnitKind::from_db(&unit_kind_raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    3,
                    Type::Text,
                    format!("invalid parsed_unit.unit_kind {unit_kind_raw:?}").into(),
                )
            })?;
            Ok(FtsSourceRow {
                occurrence_id: r.get(0)?,
                normalized_path: r.get(1)?,
                qualified_name: r.get(2)?,
                unit_kind,
                local_name: r.get(4)?,
                blob_id: r.get(5)?,
                file_revision_id: r.get(6)?,
                span_start: r.get(7)?,
                span_end: r.get(8)?,
                language: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The number of occurrences recorded for `generation_id` (spec 03 §2.4,
/// T08-03) — a cheap `COUNT(*)`, served by the same `occurrence_by_gen` index
/// as [`occurrence_ids_for_generation`], without materializing any row. Used
/// by FTS validation (`cache::validate`) both as the per-search count-check
/// input and as the fresh (never-stale) rebuild-cost estimate.
pub fn occurrence_count_for_generation(
    conn: &Connection,
    generation_id: &str,
) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM generation_unit_occurrence WHERE generation_id = ?1",
        params![generation_id],
        |r| r.get(0),
    )
}

/// The skip reason recorded for `normalized_path` in `generation_id`, if the file
/// was skipped (spec 03 §2.4).
///
/// A stored value outside the CHECK domain (corruption) surfaces as
/// [`FromSqlConversionFailure`](rusqlite::Error::FromSqlConversionFailure), the
/// same idiom the registry uses for `worktree.state` — never a silent default.
pub fn skip_reason(
    conn: &Connection,
    generation_id: &str,
    normalized_path: &str,
) -> rusqlite::Result<Option<SkipReason>> {
    conn.query_row(
        "SELECT reason FROM skipped_file \
         WHERE generation_id = ?1 AND normalized_path = ?2",
        params![generation_id, normalized_path],
        |r| {
            let raw: String = r.get(0)?;
            SkipReason::from_db(&raw).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    0,
                    Type::Text,
                    format!("invalid skipped_file.reason {raw:?}").into(),
                )
            })
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_reason_roundtrips() {
        for r in [
            SkipReason::Binary,
            SkipReason::Lfs,
            SkipReason::Huge,
            SkipReason::Secret,
            SkipReason::Ignored,
            SkipReason::Encoding,
        ] {
            assert_eq!(SkipReason::from_db(r.as_str()), Some(r));
        }
        assert_eq!(SkipReason::from_db("vendored"), None);
    }

    #[test]
    fn edge_resolution_roundtrips() {
        for e in [
            EdgeResolution::Heuristic,
            EdgeResolution::Syntax,
            EdgeResolution::Lsp,
        ] {
            assert_eq!(EdgeResolution::from_db(e.as_str()), Some(e));
        }
        assert_eq!(EdgeResolution::from_db("manual"), None);
    }

    /// A store whose `skipped_file.reason` somehow holds a value outside the CHECK
    /// domain (corruption) must surface a typed conversion error from
    /// [`skip_reason`], not a silent default. A minimal constraint-free table
    /// injects the bad value.
    #[test]
    fn skip_reason_rejects_corrupt_enum() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE skipped_file \
               (generation_id TEXT, normalized_path TEXT, reason TEXT, content_hash TEXT);\n\
             INSERT INTO skipped_file VALUES ('g', 'a.rs', 'vendored', NULL);",
        )
        .expect("seed corrupt row");

        let bad = skip_reason(&conn, "g", "a.rs");
        assert!(
            matches!(bad, Err(Error::FromSqlConversionFailure(0, Type::Text, _))),
            "corrupt reason → typed conversion failure, got {bad:?}",
        );
        // An absent (generation, path) is a clean `None`.
        assert_eq!(skip_reason(&conn, "g", "missing.rs").expect("read"), None);
    }

    /// [`occurrence_ids_for_generation`] returns exactly the ids recorded for that
    /// generation, sorted, and never another generation's rows.
    #[test]
    fn occurrence_ids_for_generation_scopes_and_sorts() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE generation_unit_occurrence \
               (occurrence_id TEXT, generation_id TEXT, normalized_path TEXT, unit_id TEXT);\n\
             CREATE INDEX occurrence_by_gen ON generation_unit_occurrence(generation_id);",
        )
        .expect("seed schema");

        // Empty generation → empty list.
        assert_eq!(
            occurrence_ids_for_generation(&conn, "g-empty").expect("read"),
            Vec::<String>::new()
        );

        conn.execute_batch(
            "INSERT INTO generation_unit_occurrence VALUES \
               ('cc', 'g1', 'b.rs', 'u1'), \
               ('aa', 'g1', 'a.rs', 'u2'), \
               ('bb', 'g1', 'c.rs', 'u3'), \
               ('zz', 'g2', 'a.rs', 'u4');",
        )
        .expect("seed rows");

        assert_eq!(
            occurrence_ids_for_generation(&conn, "g1").expect("read"),
            vec!["aa".to_string(), "bb".to_string(), "cc".to_string()],
            "sorted ascending, scoped to g1"
        );
        assert_eq!(
            occurrence_ids_for_generation(&conn, "g2").expect("read"),
            vec!["zz".to_string()]
        );
    }

    /// [`occurrence_count_for_generation`] counts only the target generation's
    /// rows, and is `0` (not an error) for a generation with none.
    #[test]
    fn occurrence_count_for_generation_scopes_correctly() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE generation_unit_occurrence \
               (occurrence_id TEXT, generation_id TEXT, normalized_path TEXT, unit_id TEXT);\n\
             CREATE INDEX occurrence_by_gen ON generation_unit_occurrence(generation_id);",
        )
        .expect("seed schema");

        assert_eq!(
            occurrence_count_for_generation(&conn, "g-empty").expect("read"),
            0
        );

        conn.execute_batch(
            "INSERT INTO generation_unit_occurrence VALUES \
               ('aa', 'g1', 'a.rs', 'u1'), \
               ('bb', 'g1', 'b.rs', 'u2'), \
               ('cc', 'g2', 'a.rs', 'u3');",
        )
        .expect("seed rows");

        assert_eq!(
            occurrence_count_for_generation(&conn, "g1").expect("read"),
            2
        );
        assert_eq!(
            occurrence_count_for_generation(&conn, "g2").expect("read"),
            1
        );
        assert_eq!(
            occurrence_count_for_generation(&conn, "g-missing").expect("read"),
            0
        );
    }

    /// A minimal in-memory `generation_unit_occurrence ⋈ parsed_unit ⋈ content_blob`
    /// schema — [`occurrences_for_fts`] only reads these three tables.
    fn seed_fts_source_schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE generation_unit_occurrence \
               (occurrence_id TEXT, generation_id TEXT, normalized_path TEXT, \
                unit_id TEXT, qualified_name TEXT);\n\
             CREATE TABLE parsed_unit \
               (unit_id TEXT, file_revision_id TEXT, unit_kind TEXT, blob_id TEXT, \
                span_start INTEGER, span_end INTEGER, local_name TEXT);\n\
             CREATE TABLE content_blob (blob_id TEXT, language TEXT);",
        )
        .expect("seed schema");
    }

    /// [`occurrences_for_fts`] joins all three tables, scopes by `generation_id`,
    /// and orders by `occurrence_id`; an empty generation yields an empty list.
    #[test]
    fn occurrences_for_fts_joins_scopes_and_orders() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        seed_fts_source_schema(&conn);

        assert_eq!(
            occurrences_for_fts(&conn, "g-empty").expect("read"),
            Vec::new()
        );

        conn.execute_batch(
            "INSERT INTO content_blob VALUES ('blob-1', 'rust'), ('blob-2', 'rust');\n\
             INSERT INTO parsed_unit VALUES \
               ('u1', 'rev-1', 'symbol', 'blob-1', 0, 10, 'foo'), \
               ('u2', 'rev-2', 'file', 'blob-2', 0, 20, NULL);\n\
             INSERT INTO generation_unit_occurrence VALUES \
               ('occ-b', 'g1', 'b.rs', 'u2', NULL), \
               ('occ-a', 'g1', 'a.rs', 'u1', NULL), \
               ('occ-other', 'g2', 'a.rs', 'u1', NULL);",
        )
        .expect("seed rows");

        let rows = occurrences_for_fts(&conn, "g1").expect("read");
        assert_eq!(rows.len(), 2, "scoped to g1, not g2");
        assert_eq!(rows[0].occurrence_id, "occ-a", "ordered by occurrence_id");
        assert_eq!(rows[0].normalized_path, "a.rs");
        assert_eq!(rows[0].unit_kind, UnitKind::Symbol);
        assert_eq!(rows[0].local_name.as_deref(), Some("foo"));
        assert_eq!(rows[0].blob_id, "blob-1");
        assert_eq!(rows[0].file_revision_id, "rev-1");
        assert_eq!((rows[0].span_start, rows[0].span_end), (0, 10));
        assert_eq!(rows[0].language, "rust");
        assert_eq!(rows[0].qualified_name, None);

        assert_eq!(rows[1].occurrence_id, "occ-b");
        assert_eq!(rows[1].unit_kind, UnitKind::File);
    }

    /// A stored `parsed_unit.unit_kind` outside the CHECK domain (corruption)
    /// surfaces as a typed conversion error, never a silent default (same idiom
    /// as [`skip_reason_rejects_corrupt_enum`]).
    #[test]
    fn occurrences_for_fts_rejects_corrupt_unit_kind() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        seed_fts_source_schema(&conn);
        conn.execute_batch(
            "INSERT INTO content_blob VALUES ('blob-1', 'rust');\n\
             INSERT INTO parsed_unit VALUES \
               ('u1', 'rev-1', 'bogus_kind', 'blob-1', 0, 10, NULL);\n\
             INSERT INTO generation_unit_occurrence VALUES \
               ('occ-a', 'g1', 'a.rs', 'u1', NULL);",
        )
        .expect("seed corrupt row");

        let bad = occurrences_for_fts(&conn, "g1");
        assert!(
            matches!(bad, Err(Error::FromSqlConversionFailure(3, Type::Text, _))),
            "corrupt unit_kind → typed conversion failure, got {bad:?}",
        );
    }

    // A realistic occurrence tuple: a UUIDv7-like generation id, a normalized
    // path, and a UUIDv7-like unit id.
    const G: &str = "018f0000-0000-7000-8000-000000000001";
    const P: &str = "src/main.rs";
    const U: &str = "018f0000-0000-7000-8000-0000000000a0";

    /// `occurrence_id` only *forwards* its three fields, in the spec-fixed order
    /// (spec 03 §1.2 table row `…/occurrence_id`), to the domain hasher — and pins
    /// the resulting digest so a field reorder or domain drift at this layer is
    /// caught. The digest is the 64 lowercase hex of BLAKE3.
    #[test]
    fn occurrence_id_matches_domain_hash_golden() {
        let id = occurrence_id(G, P, U);

        // Format: 64 lowercase hex characters.
        assert_eq!(id.len(), 64, "BLAKE3 hex digest");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "lowercase hex only: {id}",
        );

        // Forwarding: identical to hashing the fields directly in table order.
        assert_eq!(
            id,
            hash(
                Domain::OccurrenceId,
                &[G.as_bytes(), P.as_bytes(), U.as_bytes()]
            ),
            "occurrence_id forwards (generation_id, normalized_path, unit_id)",
        );

        // Golden: pins the exact digest against accidental field-order/domain drift.
        assert_eq!(
            id, "79e86eca6244de2766b201476681c17fd9476290ee1d30d1b3f93c74f82891c5",
            "occurrence_id golden (spec 03 §1.2 `…/occurrence_id`)",
        );
    }

    /// The id depends only on its own tuple: it is stable under retry (same inputs
    /// → same output) and independent of the order in which occurrences are
    /// processed (spec 03 §1.2 `[FIXED]`, the property T05-03's builder relies on).
    #[test]
    fn occurrence_id_is_retry_and_order_independent() {
        // Retry stability.
        assert_eq!(occurrence_id(G, P, U), occurrence_id(G, P, U));

        // A list of distinct occurrences, hashed in two different processing
        // orders, yields byte-identical per-tuple ids.
        let tuples = [
            (G, "src/a.rs", U),
            (G, "src/b.rs", "018f0000-0000-7000-8000-0000000000b1"),
            ("018f0000-0000-7000-8000-000000000002", P, U),
        ];
        let forward: Vec<String> = tuples
            .iter()
            .map(|(g, p, u)| occurrence_id(g, p, u))
            .collect();
        let reverse: Vec<String> = tuples
            .iter()
            .rev()
            .map(|(g, p, u)| occurrence_id(g, p, u))
            .collect();
        for (i, (g, p, u)) in tuples.iter().enumerate() {
            assert_eq!(forward[i], occurrence_id(g, p, u));
            // The same tuple hashes identically regardless of position.
            assert_eq!(forward[i], reverse[tuples.len() - 1 - i]);
        }

        // Distinct tuples → distinct ids (no collisions across the realistic set).
        let mut sorted = forward.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            forward.len(),
            "distinct tuples → distinct ids"
        );
    }

    /// Length-prefixed field framing (spec 03 §1.2) makes field boundaries
    /// unambiguous: shifting a byte across a field boundary changes the digest,
    /// so `occurrence_id` cannot be spoofed by concatenation collisions.
    #[test]
    fn occurrence_id_field_boundaries_are_unambiguous() {
        assert_ne!(
            occurrence_id("ab", "c", U),
            occurrence_id("a", "bc", U),
            "moving a byte across the generation_id/normalized_path boundary",
        );
        assert_ne!(
            occurrence_id(G, "pq", "r"),
            occurrence_id(G, "p", "qr"),
            "moving a byte across the normalized_path/unit_id boundary",
        );
    }
}
