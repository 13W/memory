//! Content-shared, path-independent code tables (spec 03 §2.3): `file_revision`,
//! `content_blob`, `parsed_unit`.
//!
//! These rows are shared by *content*: a `file_revision` is keyed by
//! `(content_hash, parser_fingerprint)`, a `content_blob` by its own derived
//! `blob_id`, and a `parsed_unit` hangs off a revision. None of them may carry a
//! path-, context-, or generation-specific field (spec 01 §5.1); the path a
//! revision is mounted at lives only in `generation_file` (see the `membership`
//! module).
//!
//! Following the registry primitives, write operations take a [`Transaction`] so
//! they compose inside a single
//! [`StateWriter::transaction`](crate::StateWriter::transaction) closure and read
//! operations take a [`Connection`]. Ids/hashes are minted by the caller and
//! passed in as strings — these low-level ops store exactly what they are given;
//! the create-or-reuse-by-key and hashing/encoding/compression logic lives in the
//! sibling [`source`](super::source) module (T03-03).

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::normalize::DerivedContentBlob;

/// How `file_revision.source_blob` is stored (spec 03 §2.3
/// `file_revision.source_compression`).
///
/// This enum is only the stored tag; the zstd round-trip that produces and reads
/// it lives in [`source::prepare_source`](super::source::prepare_source) /
/// [`source::decode_source`](super::source::decode_source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceCompression {
    /// The blob is the exact original bytes, uncompressed.
    None,
    /// The blob is zstd-compressed; `source_size` remains the uncompressed size.
    Zstd,
}

impl SourceCompression {
    /// The stored `file_revision.source_compression` value.
    pub fn as_str(self) -> &'static str {
        match self {
            SourceCompression::None => "none",
            SourceCompression::Zstd => "zstd",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "none" => Some(SourceCompression::None),
            "zstd" => Some(SourceCompression::Zstd),
            _ => None,
        }
    }
}

/// The newline convention detected in a file revision (spec 03 §2.3
/// `file_revision.newline_style`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewlineStyle {
    /// `\n` only.
    Lf,
    /// `\r\n` only.
    Crlf,
    /// A mix of `\n` and `\r\n`.
    Mixed,
}

impl NewlineStyle {
    /// The stored `file_revision.newline_style` value.
    pub fn as_str(self) -> &'static str {
        match self {
            NewlineStyle::Lf => "lf",
            NewlineStyle::Crlf => "crlf",
            NewlineStyle::Mixed => "mixed",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "lf" => Some(NewlineStyle::Lf),
            "crlf" => Some(NewlineStyle::Crlf),
            "mixed" => Some(NewlineStyle::Mixed),
            _ => None,
        }
    }
}

/// The kind of a parsed unit (spec 03 §2.3 `parsed_unit.unit_kind`). All kinds
/// are indexed (v1 parity, spec 06 §2.1) `[FIXED]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    /// A named symbol (function, class, …).
    Symbol,
    /// The whole file as one unit.
    File,
    /// A configuration section (e.g. a TOML/YAML block).
    ConfigSection,
    /// A section of prose/text.
    TextSection,
    /// A size-bounded fallback chunk when no finer structure is available.
    FallbackChunk,
}

impl UnitKind {
    /// The stored `parsed_unit.unit_kind` value.
    pub fn as_str(self) -> &'static str {
        match self {
            UnitKind::Symbol => "symbol",
            UnitKind::File => "file",
            UnitKind::ConfigSection => "config_section",
            UnitKind::TextSection => "text_section",
            UnitKind::FallbackChunk => "fallback_chunk",
        }
    }

    /// Parse a stored value; `None` for anything the CHECK constraint forbids.
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "symbol" => Some(UnitKind::Symbol),
            "file" => Some(UnitKind::File),
            "config_section" => Some(UnitKind::ConfigSection),
            "text_section" => Some(UnitKind::TextSection),
            "fallback_chunk" => Some(UnitKind::FallbackChunk),
            _ => None,
        }
    }
}

/// A row to insert into `file_revision` (spec 03 §2.3).
///
/// The exact original bytes travel in `source_blob`; `content_hash` is
/// `H(file_content)` and `parser_fingerprint` is the canonical §2.3.1 string —
/// together the revision's reuse key. `source_size` is the *uncompressed* byte
/// count regardless of `compression`.
#[derive(Debug, Clone, Copy)]
pub struct NewFileRevision<'a> {
    /// Caller-minted UUIDv7.
    pub file_revision_id: &'a str,
    /// `H(file_content)` over the raw file bytes (spec 03 §1.2).
    pub content_hash: &'a str,
    /// Canonical parser-fingerprint string (spec 03 §2.3.1).
    pub parser_fingerprint: &'a str,
    /// The exact original bytes (possibly zstd-compressed per `compression`).
    pub source_blob: &'a [u8],
    /// Whether `source_blob` is compressed.
    pub compression: SourceCompression,
    /// Source encoding label, e.g. `"utf-8"`.
    pub source_encoding: &'a str,
    /// Detected newline convention.
    pub newline_style: NewlineStyle,
    /// Uncompressed source size in bytes.
    pub source_size: i64,
}

/// Insert a `file_revision` row (spec 03 §2.3).
///
/// The `UNIQUE (content_hash, parser_fingerprint)` constraint makes a second
/// insert with the same reuse key fail with a
/// [`ConstraintViolation`](rusqlite::ErrorCode::ConstraintViolation) that rolls
/// the transaction back; the create-or-reuse wrapper that looks the key up first
/// and turns a hit into a reuse is
/// [`source::create_or_reuse_file_revision`](super::source::create_or_reuse_file_revision).
pub fn insert_file_revision(
    tx: &Transaction<'_>,
    rev: &NewFileRevision<'_>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO file_revision \
           (file_revision_id, content_hash, parser_fingerprint, source_blob, \
            source_compression, source_encoding, newline_style, source_size, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            rev.file_revision_id,
            rev.content_hash,
            rev.parser_fingerprint,
            rev.source_blob,
            rev.compression.as_str(),
            rev.source_encoding,
            rev.newline_style.as_str(),
            rev.source_size,
            now_ms,
        ],
    )?;
    Ok(())
}

/// A row to insert into `content_blob` (spec 03 §2.3).
///
/// The blob carries identity + metadata only; its normalized text lives in the
/// rebuildable `normalized_text_cache` (spec 03 §4.2, T03-04). `blob_id` is the
/// derived `H(content_blob …)` (spec 03 §1.2).
#[derive(Debug, Clone, Copy)]
pub struct NewContentBlob<'a> {
    /// Derived `blob_id` (`H(content_blob …)`).
    pub blob_id: &'a str,
    /// Language label.
    pub language: &'a str,
    /// Normalization/algo versions that fed the `blob_id` derivation.
    pub algo_version: i64,
    /// Normalization version (spec 03 §1.2 field of the `content_blob` domain).
    pub normalization_version: i64,
}

/// Insert a `content_blob` row (spec 03 §2.3).
pub fn insert_content_blob(
    tx: &Transaction<'_>,
    blob: &NewContentBlob<'_>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO content_blob \
           (blob_id, language, algo_version, normalization_version, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            blob.blob_id,
            blob.language,
            blob.algo_version,
            blob.normalization_version,
            now_ms,
        ],
    )?;
    Ok(())
}

/// The outcome of [`create_or_reuse_content_blob`]: whether an existing blob was
/// reused or a new one created. Either way [`BlobOutcome::id`] is the `blob_id`
/// to reference (and to key the `normalized_text_cache` row on).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobOutcome {
    /// A `content_blob` with this `blob_id` already existed.
    Reused(String),
    /// A new `content_blob` row was inserted.
    Created(String),
}

impl BlobOutcome {
    /// The `blob_id` to reference, regardless of reuse vs create.
    pub fn id(&self) -> &str {
        match self {
            BlobOutcome::Reused(id) | BlobOutcome::Created(id) => id,
        }
    }

    /// Whether this outcome inserted a new row.
    pub fn is_created(&self) -> bool {
        matches!(self, BlobOutcome::Created(_))
    }
}

/// Whether a `content_blob` row already exists for `blob_id` (its primary key).
pub fn content_blob_exists(conn: &Connection, blob_id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM content_blob WHERE blob_id = ?1",
        params![blob_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
}

/// Reuse the existing `content_blob` for `derived.blob_id`, or insert a new one
/// (spec 03 §2.3, §1.2). Materializes the blob *identity* in `state.sqlite`; the
/// normalized text it names lives only in `normalized_text_cache` (spec 03 §4.2).
///
/// The `blob_id` is the content-derived primary key, so it *is* the reuse key:
/// two units with identical normalized text (same `algo_version`/`language`/
/// `normalization_version`) share one `content_blob` row. Like
/// [`create_or_reuse_file_revision`](super::source::create_or_reuse_file_revision),
/// a plain SELECT-then-INSERT is race-free under the single bounded writer (spec
/// 02 §5) and idempotent on retry — a replay returns [`BlobOutcome::Reused`] with
/// no duplicate.
pub fn create_or_reuse_content_blob(
    tx: &Transaction<'_>,
    derived: &DerivedContentBlob,
    language: &str,
    now_ms: i64,
) -> rusqlite::Result<BlobOutcome> {
    if content_blob_exists(tx, &derived.blob_id)? {
        return Ok(BlobOutcome::Reused(derived.blob_id.clone()));
    }
    insert_content_blob(
        tx,
        &NewContentBlob {
            blob_id: &derived.blob_id,
            language,
            algo_version: derived.algo_version,
            normalization_version: derived.normalization_version,
        },
        now_ms,
    )?;
    Ok(BlobOutcome::Created(derived.blob_id.clone()))
}

/// A row to insert into `parsed_unit` (spec 03 §2.3).
///
/// `syntax_locator` is the canonical serialization with **no path**;
/// `span_start`/`span_end` are byte offsets into the exact `source_blob`
/// (`span_end >= span_start` is a CHECK constraint). `parent_unit_id` is a
/// self-reference for nested units.
#[derive(Debug, Clone, Copy)]
pub struct NewParsedUnit<'a> {
    /// Caller-minted UUIDv7.
    pub unit_id: &'a str,
    /// The owning revision.
    pub file_revision_id: &'a str,
    /// The unit's kind.
    pub unit_kind: UnitKind,
    /// Canonical, path-free syntax locator serialization.
    pub syntax_locator: &'a str,
    /// The content blob this unit's normalized text derives from.
    pub blob_id: &'a str,
    /// Byte offset of the unit's start in `source_blob`.
    pub span_start: i64,
    /// Byte offset of the unit's end in `source_blob` (`>= span_start`).
    pub span_end: i64,
    /// Optional local (unqualified) name.
    pub local_name: Option<&'a str>,
    /// Optional language-level kind label (`fn`/`class`/…).
    pub kind: Option<&'a str>,
    /// Optional parent unit for nesting.
    pub parent_unit_id: Option<&'a str>,
}

/// Insert a `parsed_unit` row (spec 03 §2.3).
///
/// Fails with a [`ConstraintViolation`](rusqlite::ErrorCode::ConstraintViolation)
/// on a `span_end < span_start` CHECK violation, an unknown `file_revision_id`/
/// `blob_id`/`parent_unit_id` foreign key, or a duplicate
/// `(file_revision_id, unit_kind, syntax_locator, span_start, span_end)`.
pub fn insert_parsed_unit(tx: &Transaction<'_>, unit: &NewParsedUnit<'_>) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO parsed_unit \
           (unit_id, file_revision_id, unit_kind, syntax_locator, blob_id, \
            span_start, span_end, local_name, kind, parent_unit_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            unit.unit_id,
            unit.file_revision_id,
            unit.unit_kind.as_str(),
            unit.syntax_locator,
            unit.blob_id,
            unit.span_start,
            unit.span_end,
            unit.local_name,
            unit.kind,
            unit.parent_unit_id,
        ],
    )?;
    Ok(())
}

/// The outcome of [`create_or_reuse_parsed_unit`]: whether an existing unit was
/// reused or a new one created. Either way [`ParsedUnitOutcome::id`] is the
/// `unit_id` to reference (as a `parent_unit_id` or an occurrence's `unit_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedUnitOutcome {
    /// A `parsed_unit` matching the natural key already existed; its `unit_id`.
    Reused(String),
    /// A new `parsed_unit` row was inserted under the caller's `unit_id`.
    Created(String),
}

impl ParsedUnitOutcome {
    /// The `unit_id` to reference, regardless of reuse vs create.
    pub fn id(&self) -> &str {
        match self {
            ParsedUnitOutcome::Reused(id) | ParsedUnitOutcome::Created(id) => id,
        }
    }

    /// Whether this outcome inserted a new row.
    pub fn is_created(&self) -> bool {
        matches!(self, ParsedUnitOutcome::Created(_))
    }
}

/// The `unit_id` for a `parsed_unit` natural key, if one exists (spec 03 §2.3).
///
/// The natural key is the table's `UNIQUE (file_revision_id, unit_kind,
/// syntax_locator, span_start, span_end)`. This is the read half of
/// [`create_or_reuse_parsed_unit`]; here it is a plain unique-key lookup.
pub fn parsed_unit_id_by_natural_key(
    conn: &Connection,
    file_revision_id: &str,
    unit_kind: UnitKind,
    syntax_locator: &str,
    span_start: i64,
    span_end: i64,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT unit_id FROM parsed_unit \
         WHERE file_revision_id = ?1 AND unit_kind = ?2 AND syntax_locator = ?3 \
           AND span_start = ?4 AND span_end = ?5",
        params![
            file_revision_id,
            unit_kind.as_str(),
            syntax_locator,
            span_start,
            span_end,
        ],
        |r| r.get(0),
    )
    .optional()
}

/// Every `unit_id` of a file revision, in a deterministic order (spec 03 §2.3).
///
/// Ordered by `(span_start, span_end, unit_kind, unit_id)` so the result is stable
/// regardless of insertion order. This is the read the reconcile generation builder
/// (group 05) uses to re-derive a **reused** revision's occurrences without
/// re-parsing: a `file_revision` shared by content across generations already has
/// its `parsed_unit` rows, so the builder reads their ids and mints a
/// per-generation `occurrence` for each (occurrence identity is order-independent,
/// so the ordering here is only for tidy, reproducible output).
pub fn parsed_units_for_revision(
    conn: &Connection,
    file_revision_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT unit_id FROM parsed_unit \
         WHERE file_revision_id = ?1 \
         ORDER BY span_start, span_end, unit_kind, unit_id",
    )?;
    let ids = stmt
        .query_map(params![file_revision_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// Reuse the existing `parsed_unit` for `unit`'s natural key, or insert a new one
/// under `unit.unit_id` (spec 03 §2.3, 06 §2 structural sharing).
///
/// The reuse key is the `UNIQUE (file_revision_id, unit_kind, syntax_locator,
/// span_start, span_end)` tuple — not `unit_id`, which is a fresh UUIDv7 the caller
/// mints unconditionally and which is consumed only on the create path. Like
/// [`create_or_reuse_content_blob`] and
/// [`create_or_reuse_file_revision`](super::source::create_or_reuse_file_revision),
/// the plain SELECT-then-INSERT is race-free under the single bounded writer (spec
/// 02 §5) and idempotent on retry: a replay of the same parse finds the row and
/// returns [`ParsedUnitOutcome::Reused`] with no duplicate, so re-persisting an
/// unchanged file revision reuses every unit id (the stability the deterministic
/// `occurrence_id` of group 05 builds on).
pub fn create_or_reuse_parsed_unit(
    tx: &Transaction<'_>,
    unit: &NewParsedUnit<'_>,
) -> rusqlite::Result<ParsedUnitOutcome> {
    if let Some(existing) = parsed_unit_id_by_natural_key(
        tx,
        unit.file_revision_id,
        unit.unit_kind,
        unit.syntax_locator,
        unit.span_start,
        unit.span_end,
    )? {
        return Ok(ParsedUnitOutcome::Reused(existing));
    }
    insert_parsed_unit(tx, unit)?;
    Ok(ParsedUnitOutcome::Created(unit.unit_id.to_string()))
}

/// The `file_revision_id` for a `(content_hash, parser_fingerprint)` reuse key,
/// if one exists (spec 03 §2.3).
///
/// This is the read half of the create-or-reuse logic; the completing wrapper is
/// [`source::create_or_reuse_file_revision`](super::source::create_or_reuse_file_revision).
/// Here it is a plain unique-key lookup.
pub fn file_revision_id_by_content_key(
    conn: &Connection,
    content_hash: &str,
    parser_fingerprint: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT file_revision_id FROM file_revision \
         WHERE content_hash = ?1 AND parser_fingerprint = ?2",
        params![content_hash, parser_fingerprint],
        |r| r.get(0),
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_compression_roundtrips() {
        for c in [SourceCompression::None, SourceCompression::Zstd] {
            assert_eq!(SourceCompression::from_db(c.as_str()), Some(c));
        }
        assert_eq!(SourceCompression::from_db("gzip"), None);
    }

    #[test]
    fn newline_style_roundtrips() {
        for n in [NewlineStyle::Lf, NewlineStyle::Crlf, NewlineStyle::Mixed] {
            assert_eq!(NewlineStyle::from_db(n.as_str()), Some(n));
        }
        assert_eq!(NewlineStyle::from_db("cr"), None);
    }

    #[test]
    fn unit_kind_roundtrips() {
        for k in [
            UnitKind::Symbol,
            UnitKind::File,
            UnitKind::ConfigSection,
            UnitKind::TextSection,
            UnitKind::FallbackChunk,
        ] {
            assert_eq!(UnitKind::from_db(k.as_str()), Some(k));
        }
        assert_eq!(UnitKind::from_db("module"), None);
    }
}
