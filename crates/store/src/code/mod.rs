//! Code-storage tables in `state.sqlite` — the exact-source side of the model
//! (spec 03 §2.3–2.4, 06 §2, 12 §5).
//!
//! This module owns the third numbered migration ([`SCHEMA_V3`]) and the typed
//! repositories over its tables. Two spec-01-§5 identity ladders shape it:
//!
//! - **Content-shared, path-independent** (§2.3): `file_revision`,
//!   `content_blob`, `parsed_unit`. A row here is shared by *content* across every
//!   path and generation that contains it, so it MUST NOT carry a path-, context-,
//!   or generation-specific field (spec 01 §5.1). `file_revision.source_blob` is
//!   the *exact original bytes* — the strict source-blob invariant (spec 12 §5):
//!   no `source_blob` ⇒ the file is not part of the canonical indexed generation.
//! - **Generation membership, path-dependent** (§2.4): `generation_file`,
//!   `skipped_file`, `generation_unit_occurrence`, `unresolved_reference`,
//!   `resolved_graph_edge`. These are the *only* legitimate home for a
//!   `normalized_path`/`display_path` outside the two path ledgers (spec 01 §5.1:
//!   "Everything path/generation-dependent lives only in
//!   `generation_unit_occurrence`, `resolved_graph_edge`, `generation_file`, and
//!   the FTS projection"). The structural source-blob invariant is enforced by the
//!   composite foreign key `generation_unit_occurrence(generation_id,
//!   normalized_path) → generation_file(generation_id, normalized_path)`: an
//!   occurrence can only exist on a *member* file, and a member file always has a
//!   `file_revision` with a non-null `source_blob`. Skipped files
//!   (`skipped_file`) never get occurrences (spec 06 §2.2, 12 §5).
//!
//! Scope note: T03-01 shipped the schema plus the low-level typed insert/read
//! repositories with their exact constraints and typed enums, storing exactly the
//! ids/hashes/bytes it was handed. T03-03 adds the [`source`] ingestion layer —
//! content hashing, encoding/newline detection, optional zstd with exact byte
//! round-trip, and the create-or-reuse-by `(content_hash, parser_fingerprint)`
//! wrapper. T03-04 adds the [`normalize`] layer — versioned text normalization,
//! the `content_blob` identity derivation (`H(content_blob …)`), and
//! `create_or_reuse_content_blob`; the normalized text it derives lives in the
//! rebuildable `normalized_text_cache` (spec 03 §4.2, see the `cache::text`
//! module). File classification is T03-02. T05-01 adds the deterministic
//! [`occurrence_id`] derivation (`H(occurrence_id: generation_id, normalized_path,
//! unit_id)`, spec 03 §1.2); the generation *builder* that mints and persists
//! occurrences with it is T05-03. Callers still supply ids/`now_ms` as the
//! registry primitives do, keeping the clock and entropy out of the write path.

mod membership;
mod normalize;
mod revision;
mod source;

pub use membership::{
    EdgeResolution, NewOccurrence, NewResolvedEdge, NewUnresolvedReference, SkipReason,
    delete_unresolved_references_for_revision, insert_generation_file, insert_occurrence,
    insert_resolved_edge, insert_skipped_file, insert_unresolved_reference, member_file_revision,
    occurrence_id, skip_reason,
};
pub use normalize::{
    ALGO_VERSION, DerivedContentBlob, NORMALIZATION_VERSION, content_blob_id, derive_content_blob,
    normalize,
};
pub use revision::{
    BlobOutcome, NewContentBlob, NewFileRevision, NewParsedUnit, NewlineStyle, ParsedUnitOutcome,
    SourceCompression, UnitKind, content_blob_exists, create_or_reuse_content_blob,
    create_or_reuse_parsed_unit, file_revision_id_by_content_key, insert_content_blob,
    insert_file_revision, insert_parsed_unit, parsed_unit_id_by_natural_key,
    parsed_units_for_revision,
};
pub use source::{
    PreparedSource, RevisionOutcome, SOURCE_ENCODING_UTF8, SOURCE_ZSTD_LEVEL, content_hash,
    create_or_reuse_file_revision, decode_source, detect_encoding, detect_newline_style,
    prepare_source, source_bytes,
};

/// Version-3 migration DDL: the code-storage side (spec 03 §2.3–2.4).
///
/// Byte-exact reproduction of the §2.3 (content-shared) and §2.4 (generation
/// membership) blocks — `file_revision`, `content_blob`, `parsed_unit`,
/// `generation_file`, `skipped_file`, `generation_unit_occurrence` (+ its
/// `occurrence_by_gen`/`occurrence_by_unit` indexes), `unresolved_reference`
/// (+ `unresolved_by_rev`), and `resolved_graph_edge`. Referenced by
/// [`crate::migrate::ALL`] as migration version 3.
///
/// Table order matters: a `REFERENCES` target must already exist when its child
/// table is created (`generation_file` needs `file_revision`;
/// `generation_unit_occurrence`'s composite FK needs `generation_file`'s
/// `(generation_id, normalized_path)` primary key; `resolved_graph_edge` needs
/// `generation_unit_occurrence`). The spec's declaration order already satisfies
/// this, and `generation` itself comes from [`SCHEMA_V2`](crate::registry).
///
/// **Frozen once shipped.** Like [`SCHEMA_V1`](crate::registry) /
/// [`SCHEMA_V2`](crate::registry), the migration checksum is the SHA-256 of this
/// text (see [`crate::migrate::Migration::checksum`]); any edit — even whitespace
/// or a comment — changes the checksum and trips
/// [`ChecksumDrift`](crate::migrate::MigrationError::ChecksumDrift) on an existing
/// store. Future schema changes are new numbered migrations, never an edit here.
pub(crate) const SCHEMA_V3: &str = "\
CREATE TABLE file_revision (
  file_revision_id    TEXT PRIMARY KEY,               -- UUIDv7
  content_hash        TEXT NOT NULL,                  -- H(file_content)
  parser_fingerprint  TEXT NOT NULL,                  -- canonical string, §2.3.1
  source_blob         BLOB NOT NULL,                  -- exact original bytes [FIXED: strict invariant]
  source_compression  TEXT NOT NULL CHECK (source_compression IN ('none','zstd')),
  source_encoding     TEXT NOT NULL,                  -- e.g. 'utf-8'
  newline_style       TEXT NOT NULL CHECK (newline_style IN ('lf','crlf','mixed')),
  source_size         INTEGER NOT NULL,               -- uncompressed bytes
  created_at          INTEGER NOT NULL,
  UNIQUE (content_hash, parser_fingerprint)
);

CREATE TABLE content_blob (                           -- identity + metadata; text lives in cache
  blob_id                TEXT PRIMARY KEY,            -- H(content_blob …)
  language               TEXT NOT NULL,
  algo_version           INTEGER NOT NULL,
  normalization_version  INTEGER NOT NULL,
  created_at             INTEGER NOT NULL
);

CREATE TABLE parsed_unit (
  unit_id           TEXT PRIMARY KEY,                 -- UUIDv7
  file_revision_id  TEXT NOT NULL REFERENCES file_revision(file_revision_id),
  unit_kind         TEXT NOT NULL CHECK
    (unit_kind IN ('symbol','file','config_section','text_section','fallback_chunk')),
  syntax_locator    TEXT NOT NULL,                    -- canonical serialization, NO path
  blob_id           TEXT NOT NULL REFERENCES content_blob(blob_id),
  span_start        INTEGER NOT NULL,                 -- byte offsets into exact source_blob
  span_end          INTEGER NOT NULL CHECK (span_end >= span_start),
  local_name        TEXT,
  kind              TEXT,                             -- language-level kind (fn/class/…)
  parent_unit_id    TEXT REFERENCES parsed_unit(unit_id),
  UNIQUE (file_revision_id, unit_kind, syntax_locator, span_start, span_end)
);

CREATE TABLE generation_file (
  generation_id     TEXT NOT NULL REFERENCES generation(generation_id),
  normalized_path   TEXT NOT NULL,
  display_path      TEXT NOT NULL,
  file_revision_id  TEXT NOT NULL REFERENCES file_revision(file_revision_id),
  PRIMARY KEY (generation_id, normalized_path)
);

CREATE TABLE skipped_file (
  generation_id    TEXT NOT NULL REFERENCES generation(generation_id),
  normalized_path  TEXT NOT NULL,
  reason           TEXT NOT NULL CHECK
    (reason IN ('binary','lfs','huge','secret','ignored','encoding')),
  content_hash     TEXT,
  PRIMARY KEY (generation_id, normalized_path)
);
-- skipped files NEVER get occurrences [FIXED §10 invariant]

CREATE TABLE generation_unit_occurrence (
  occurrence_id    TEXT PRIMARY KEY,                  -- H(occurrence_id, …) deterministic
  generation_id    TEXT NOT NULL,
  normalized_path  TEXT NOT NULL,
  unit_id          TEXT NOT NULL REFERENCES parsed_unit(unit_id),
  qualified_name   TEXT,
  context_hash     TEXT,
  UNIQUE (generation_id, normalized_path, unit_id),
  -- occurrence only on a member file ⇒ (with file_revision.source_blob NOT NULL)
  -- the source-blob invariant is structural:
  FOREIGN KEY (generation_id, normalized_path)
    REFERENCES generation_file(generation_id, normalized_path)
);
CREATE INDEX occurrence_by_gen ON generation_unit_occurrence(generation_id);
CREATE INDEX occurrence_by_unit ON generation_unit_occurrence(unit_id);

CREATE TABLE unresolved_reference (                   -- parse-local, per file revision
  file_revision_id  TEXT NOT NULL REFERENCES file_revision(file_revision_id),
  source_unit_id    TEXT NOT NULL REFERENCES parsed_unit(unit_id),
  reference_text    TEXT NOT NULL,
  reference_kind    TEXT NOT NULL
);
CREATE INDEX unresolved_by_rev ON unresolved_reference(file_revision_id);

CREATE TABLE resolved_graph_edge (                    -- per generation, on occurrence IDs
  generation_id      TEXT NOT NULL REFERENCES generation(generation_id),
  src_occurrence_id  TEXT NOT NULL REFERENCES generation_unit_occurrence(occurrence_id),
  dst_occurrence_id  TEXT NOT NULL REFERENCES generation_unit_occurrence(occurrence_id),
  edge_kind          TEXT NOT NULL,                   -- import | call_heuristic | … [OPEN: final graph semantics]
  resolution         TEXT NOT NULL CHECK (resolution IN ('heuristic','syntax','lsp')),
  UNIQUE (generation_id, src_occurrence_id, dst_occurrence_id, edge_kind)
);
";
