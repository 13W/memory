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
//! T03-01 stores what it is given: `occurrence_id` is a caller-supplied string
//! here (its deterministic `H(occurrence_id, …)` derivation and the generation
//! builder are group 05). Write operations take a [`Transaction`]; reads take a
//! [`Connection`].

use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

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
}
