//! Exact source ingestion for `file_revision` (spec 03 §1.2, §2.3; 12 §5) —
//! T03-03.
//!
//! T03-01 shipped the `file_revision` table and a low-level
//! [`insert_file_revision`] that stores exactly the bytes/hashes it is handed.
//! This module turns raw file bytes into a durable revision:
//!
//! 1. **content hash** — `H(file_content)` over the *raw* bytes (spec 03 §1.2),
//!    the `content_hash` half of the reuse key;
//! 2. **encoding / newline detection** — `source_encoding` (v0: always `utf-8`,
//!    non-UTF-8 is skipped upstream by the classifier) and `newline_style`
//!    (`lf`/`crlf`/`mixed`);
//! 3. **optional zstd** — a keep-if-smaller compression policy that never grows
//!    the blob and never touches identity;
//! 4. **exact byte round-trip** — [`source_bytes`] reproduces the original bytes
//!    from a stored revision, decompressing when needed;
//! 5. **create-or-reuse** — [`create_or_reuse_file_revision`] returns the
//!    existing revision for a `(content_hash, parser_fingerprint)` pair or inserts
//!    a new one, atomically within one transaction.
//!
//! ## Layering `[SPEC]`
//!
//! [`prepare_source`] is a pure, CPU-bound function (BLAKE3 + zstd, no clock, no
//! entropy, no DB) so callers run it *before* entering a write transaction — off
//! the single bounded-writer thread (spec 02 §5). The transaction step
//! ([`create_or_reuse_file_revision`]) then does only a fast lookup + insert. The
//! `file_revision_id` UUIDv7 and `now_ms` are minted by the caller and passed in,
//! exactly as the registry primitives do, keeping the clock and entropy out of the
//! store.
//!
//! Scope note (T03-03): the `parser_fingerprint` is an opaque caller-supplied
//! string here; building it from a real parser is T04-02. Deriving the normalized
//! text cache from these bytes is T03-04. The async `StateWriter` wrapper and the
//! deterministic `occurrence_id`/generation builder that call this primitive are
//! group 05.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use local_rag_core::identity::Domain;
use local_rag_core::identity::domain::hash;

use super::{
    NewFileRevision, NewlineStyle, SourceCompression, file_revision_id_by_content_key,
    insert_file_revision,
};

/// The zstd compression level used for `source_blob` (`[SPEC]`).
///
/// Level 3 is zstd's own default — a fast encode with a solid ratio for source
/// text. Compression is **not** part of a revision's identity (the reuse key is
/// `(content_hash, parser_fingerprint)` only) and the exact frame bytes feed no
/// hash, so this level can be re-tuned later with **no** migration: only the
/// round-trip and the uncompressed `source_size` are observable.
pub const SOURCE_ZSTD_LEVEL: i32 = 3;

/// The `source_encoding` label stored for every accepted file in v0.
///
/// v0 supports only UTF-8; non-UTF-8 content is turned into
/// `skipped_file(reason='encoding')` by the classifier (spec 03 §2.3.1 / 06 §2.1
/// `[FIXED]`) and never reaches a `file_revision`, so no transcoding happens here.
pub const SOURCE_ENCODING_UTF8: &str = "utf-8";

/// Byte-level facts derived purely from a file's raw bytes, ready to become a
/// `file_revision` row.
///
/// `source_blob` is either the exact original bytes (`compression == None`) or a
/// zstd frame of them (`compression == Zstd`); either way [`decode_source`]
/// reproduces the original exactly. `source_size` is always the *uncompressed*
/// length.
#[derive(Debug, Clone)]
pub struct PreparedSource {
    /// `H(file_content)` over the raw bytes (spec 03 §1.2), hex-64.
    pub content_hash: String,
    /// Whether `source_blob` is a zstd frame or the raw bytes.
    pub compression: SourceCompression,
    /// The bytes to store: raw, or a zstd frame that decompresses to raw.
    pub source_blob: Vec<u8>,
    /// The source encoding label ([`SOURCE_ENCODING_UTF8`] in v0).
    pub source_encoding: &'static str,
    /// The detected newline convention.
    pub newline_style: NewlineStyle,
    /// The uncompressed source size in bytes.
    pub source_size: i64,
}

/// Derive every byte-level fact for `raw`, applying the keep-if-smaller zstd
/// policy. Infallible and pure — safe to run off the writer thread.
///
/// In debug builds this asserts `raw` is valid UTF-8, documenting the invariant
/// that non-UTF-8 files are skipped by the classifier before ingestion; it never
/// re-validates or transcodes at runtime (that is the classifier's job, T03-02).
pub fn prepare_source(raw: &[u8]) -> PreparedSource {
    debug_assert!(
        std::str::from_utf8(raw).is_ok(),
        "prepare_source received non-UTF-8 bytes; the classifier must skip these \
         as skipped_file(reason='encoding') before ingestion (spec 03 §2.3.1)"
    );
    let (compression, source_blob) = encode_source(raw);
    PreparedSource {
        content_hash: content_hash(raw),
        compression,
        source_blob,
        source_encoding: SOURCE_ENCODING_UTF8,
        newline_style: detect_newline_style(raw),
        source_size: raw.len() as i64,
    }
}

/// `H(file_content)` over the exact raw bytes — the `content_hash` half of a
/// revision's reuse key (spec 03 §1.2, domain `local-rag/1/file_content`).
///
/// Hashed through the generic [`hash`] entry point by its owning task, per the
/// convention in `local_rag_core::identity::domain` (only the path/remote *lookup*
/// fingerprints get typed constructors). The single field means an empty file
/// hashes as one zero-length field — deliberately distinct from the domain-only
/// digest `hash(Domain::FileContent, &[])`.
pub fn content_hash(raw: &[u8]) -> String {
    hash(Domain::FileContent, &[raw])
}

/// The `source_encoding` label for `raw`. v0 accepts only UTF-8, so this is
/// always [`SOURCE_ENCODING_UTF8`]; the argument is taken for a stable signature
/// as more encodings become representable.
pub fn detect_encoding(_raw: &[u8]) -> &'static str {
    SOURCE_ENCODING_UTF8
}

/// Detect the newline convention of `raw` (spec 03 §2.3 `newline_style`).
///
/// A `\n` preceded by `\r` counts as CRLF; a `\n` without a preceding `\r` counts
/// as LF. Both present ⇒ [`NewlineStyle::Mixed`]. A file with no `\n` at all — an
/// empty file, a single line without a trailing newline, or a classic-Mac lone-CR
/// file (not representable in the `{lf,crlf,mixed}` enum in v0) — reports
/// [`NewlineStyle::Lf`] (the documented default). This only labels metadata; the
/// stored bytes are always exact regardless of the label.
pub fn detect_newline_style(raw: &[u8]) -> NewlineStyle {
    let mut has_crlf = false;
    let mut has_lone_lf = false;
    let mut prev = 0u8;
    for &b in raw {
        if b == b'\n' {
            if prev == b'\r' {
                has_crlf = true;
            } else {
                has_lone_lf = true;
            }
        }
        prev = b;
    }
    match (has_crlf, has_lone_lf) {
        (true, true) => NewlineStyle::Mixed,
        (true, false) => NewlineStyle::Crlf,
        // Pure LF, or no newline at all → the documented `lf` default.
        (false, _) => NewlineStyle::Lf,
    }
}

/// Compress `raw` under the keep-if-smaller policy: attempt zstd and keep the
/// frame only when it is *strictly* smaller than the raw bytes, otherwise store
/// the raw bytes uncompressed. Guarantees `source_blob` is never larger than the
/// original and that a fresh `Vec` (independent of the caller's buffer) is stored.
fn encode_source(raw: &[u8]) -> (SourceCompression, Vec<u8>) {
    match zstd::encode_all(raw, SOURCE_ZSTD_LEVEL) {
        Ok(frame) if frame.len() < raw.len() => (SourceCompression::Zstd, frame),
        // zstd declined (frame not smaller) or errored — store the exact bytes.
        _ => (SourceCompression::None, raw.to_vec()),
    }
}

/// Reproduce the exact original bytes from a stored blob and its compression tag.
///
/// The inverse of [`encode_source`]: `None` returns the bytes unchanged, `Zstd`
/// decompresses the frame. Fails only on a corrupt/truncated zstd frame, which for
/// a value read out of the canonical store means store corruption.
pub fn decode_source(compression: SourceCompression, blob: &[u8]) -> std::io::Result<Vec<u8>> {
    match compression {
        SourceCompression::None => Ok(blob.to_vec()),
        SourceCompression::Zstd => zstd::decode_all(blob),
    }
}

/// The outcome of [`create_or_reuse_file_revision`]: whether an existing revision
/// was reused or a new one created. Either way [`RevisionOutcome::id`] is the
/// `file_revision_id` to reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionOutcome {
    /// An existing revision matched the reuse key; its `file_revision_id`.
    Reused(String),
    /// A new revision was inserted under the caller's `new_id`.
    Created(String),
}

impl RevisionOutcome {
    /// The `file_revision_id` to reference, regardless of reuse vs create.
    pub fn id(&self) -> &str {
        match self {
            RevisionOutcome::Reused(id) | RevisionOutcome::Created(id) => id,
        }
    }

    /// Whether this outcome inserted a new row.
    pub fn is_created(&self) -> bool {
        matches!(self, RevisionOutcome::Created(_))
    }
}

/// Return the existing `file_revision_id` for `(content_hash, parser_fingerprint)`,
/// or insert a new revision from `prepared` under `new_id` and return it (spec 03
/// §2.3, 06 §2 structural sharing).
///
/// A plain SELECT-then-INSERT is race-free here because every write goes through
/// the single bounded-writer thread (spec 02 §5): nothing can insert the same key
/// between the lookup and the insert within this transaction. It is idempotent on
/// retry — a replayed call finds the row and returns [`RevisionOutcome::Reused`]
/// with no duplicate. `new_id` is consumed only on the create path; on reuse it is
/// ignored, so callers may mint it unconditionally. Because compression is not part
/// of identity, the same key always denotes the same content.
pub fn create_or_reuse_file_revision(
    tx: &Transaction<'_>,
    prepared: &PreparedSource,
    parser_fingerprint: &str,
    new_id: &str,
    now_ms: i64,
) -> rusqlite::Result<RevisionOutcome> {
    if let Some(existing) =
        file_revision_id_by_content_key(tx, &prepared.content_hash, parser_fingerprint)?
    {
        return Ok(RevisionOutcome::Reused(existing));
    }
    insert_file_revision(
        tx,
        &NewFileRevision {
            file_revision_id: new_id,
            content_hash: &prepared.content_hash,
            parser_fingerprint,
            source_blob: &prepared.source_blob,
            compression: prepared.compression,
            source_encoding: prepared.source_encoding,
            newline_style: prepared.newline_style,
            source_size: prepared.source_size,
        },
        now_ms,
    )?;
    Ok(RevisionOutcome::Created(new_id.to_string()))
}

/// The exact original bytes of a revision (decompressed if needed), or `None` when
/// the `file_revision_id` is unknown (spec 03 §2.3 exact-byte invariant; 09 §7
/// snippets are cut from these bytes).
///
/// A corrupt `source_compression` tag or an undecodable zstd frame — both
/// impossible through normal inserts (the CHECK constraint and [`encode_source`]
/// guarantee well-formed values) — surface as a
/// [`FromSqlConversionFailure`](rusqlite::Error::FromSqlConversionFailure) rather
/// than a panic, matching the corrupt-enum idiom elsewhere in `code`.
pub fn source_bytes(
    conn: &Connection,
    file_revision_id: &str,
) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row(
        "SELECT source_compression, source_blob FROM file_revision \
         WHERE file_revision_id = ?1",
        params![file_revision_id],
        |row| {
            let tag: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            let compression = SourceCompression::from_db(&tag).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown source_compression tag {tag:?}"),
                    )),
                )
            })?;
            decode_source(compression, &blob).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Blob,
                    Box::new(e),
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
    fn content_hash_matches_domain_hash_and_is_hex64() {
        let raw = b"fn main() {}\n";
        let digest = content_hash(raw);
        assert_eq!(digest, hash(Domain::FileContent, &[raw]));
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        );
    }

    #[test]
    fn content_hash_covers_non_ascii_and_empty() {
        // Non-ASCII multibyte UTF-8 and the empty file all hash stably & distinctly.
        let a = content_hash("héllo — café\n".as_bytes());
        let b = content_hash("日本語のソース\n".as_bytes());
        let empty = content_hash(b"");
        assert_ne!(a, b);
        assert_ne!(a, empty);
        assert_ne!(b, empty);
        // The empty file is one zero-length field, NOT the domain-only digest.
        assert_ne!(empty, hash(Domain::FileContent, &[]));
        // Deterministic under repetition.
        assert_eq!(a, content_hash("héllo — café\n".as_bytes()));
    }

    #[test]
    fn newline_lf_crlf_mixed_and_none() {
        assert_eq!(detect_newline_style(b"a\nb\n"), NewlineStyle::Lf);
        assert_eq!(detect_newline_style(b"a\r\nb\r\n"), NewlineStyle::Crlf);
        assert_eq!(detect_newline_style(b"a\r\nb\n"), NewlineStyle::Mixed);
        // No `\n` at all → the documented `lf` default.
        assert_eq!(detect_newline_style(b"no newline here"), NewlineStyle::Lf);
        assert_eq!(detect_newline_style(b""), NewlineStyle::Lf);
        // Classic-Mac lone CR has no `\n` → not representable, treated as `lf`.
        assert_eq!(detect_newline_style(b"a\rb\r"), NewlineStyle::Lf);
    }

    #[test]
    fn compress_keeps_smaller_frame() {
        // Highly compressible: 8 KiB of one byte → zstd wins.
        let raw = vec![b'x'; 8192];
        let prepared = prepare_source(&raw);
        assert_eq!(prepared.compression, SourceCompression::Zstd);
        assert!(
            prepared.source_blob.len() < prepared.source_size as usize,
            "compressed frame must be smaller than the raw bytes"
        );
        assert_eq!(prepared.source_size, 8192);
        assert_eq!(
            decode_source(prepared.compression, &prepared.source_blob).expect("decode"),
            raw
        );
    }

    #[test]
    fn compress_declines_on_incompressible() {
        // Tiny input: a zstd frame carries fixed overhead, so it is not smaller.
        let raw = b"x";
        let prepared = prepare_source(raw);
        assert_eq!(prepared.compression, SourceCompression::None);
        assert_eq!(prepared.source_blob, raw);
        assert_eq!(prepared.source_size, 1);
        assert_eq!(
            decode_source(prepared.compression, &prepared.source_blob).expect("decode"),
            raw
        );
    }

    #[test]
    fn decode_source_round_trips_both_tags() {
        // LF / CRLF / mixed / non-ASCII / empty payloads, under either tag.
        let payloads: &[&[u8]] = &[
            b"lf\nlf\n",
            b"crlf\r\ncrlf\r\n",
            b"mixed\r\nmixed\n",
            "héllo\n日本語\n".as_bytes(),
            b"",
        ];
        for raw in payloads {
            // `None` is an exact copy.
            assert_eq!(
                decode_source(SourceCompression::None, raw).expect("none decode"),
                *raw
            );
            // A real zstd frame decompresses back to the exact bytes.
            let frame = zstd::encode_all(*raw, SOURCE_ZSTD_LEVEL).expect("encode");
            assert_eq!(
                decode_source(SourceCompression::Zstd, &frame).expect("zstd decode"),
                *raw
            );
        }
    }

    #[test]
    fn prepared_source_fields_consistent() {
        for raw in [
            &b""[..],
            &b"single line, no newline"[..],
            &b"a\nb\nc\n"[..],
            &vec![b'q'; 4096][..],
        ] {
            let prepared = prepare_source(raw);
            assert_eq!(prepared.source_size, raw.len() as i64);
            assert_eq!(prepared.source_encoding, SOURCE_ENCODING_UTF8);
            assert!(
                prepared.source_blob.len() <= raw.len(),
                "keep-if-smaller policy never grows the blob over raw"
            );
            // Whatever the tag, the stored blob round-trips to the original.
            assert_eq!(
                decode_source(prepared.compression, &prepared.source_blob).expect("decode"),
                raw
            );
        }
    }
}
