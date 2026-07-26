//! Span-bounded, size-capped excerpts cut from the stored `source_blob`
//! (spec 09 §7, 12 §2) — T12-04.
//!
//! # Why the stored bytes, never the disk
//!
//! Spec 09 §7 `[FIXED]`: "Snippets are cut from the exact `source_blob` by byte
//! span — never from the live disk file (the file may have changed since the
//! generation) — reproducibility is exactly what the source-blob invariant
//! buys." A generation is a *snapshot*: an answer that quoted the live file
//! could show text that never existed at the offsets it reports, or fail
//! outright once the file is deleted. This module therefore takes bytes a
//! caller already read out of `state.sqlite` and does no I/O of its own.
//!
//! # The cap
//!
//! Spec 12 §2 `[SPEC]` sets the snippet cap at 8 KiB and `[FIXED]`s the
//! consequence: "truncation always leaves `{hash, original_size}` metadata".
//! [`cut`] honors both.
//!
//! Two subtleties the cap creates, neither of which the span itself has:
//!
//! - **UTF-8 boundaries.** Unit spans come from tree-sitter and are
//!   character-aligned by construction, but 8 KiB lands wherever it lands. A
//!   naive `&bytes[..CAP]` can split a multi-byte character, and
//!   `String::from_utf8` would then reject an otherwise perfectly good snippet —
//!   the first CJK or emoji-heavy file would lose its excerpts. The cut is moved
//!   back to the nearest boundary instead (at most three bytes).
//! - **The hash covers the *full* excerpt**, not the truncated text: the point
//!   of the metadata is to describe what was cut away, so hashing what remains
//!   would answer the wrong question.

use std::collections::HashMap;

use local_rag_core::identity::domain::truncated_excerpt;
use local_rag_protocol::{Snippet, Truncation};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{OccurrenceMetadata, rusqlite, source_bytes};

/// The snippet size cap (spec 12 §2 `[SPEC]`: "snippet 8 KiB").
pub const SNIPPET_CAP_BYTES: usize = 8 * 1024;

/// Why a span could not be turned into a snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnippetError {
    /// The span lies (partly) outside the stored revision — a corrupt or
    /// mismatched `file_revision`, since a `parsed_unit`'s span is derived from
    /// the very bytes it is stored against.
    SpanOutOfRange {
        /// The requested span.
        span: [i64; 2],
        /// The revision's actual byte length.
        revision_len: usize,
    },
    /// The span's bytes are not valid UTF-8. The file classified as text
    /// (a binary one would be `skipped_file`, spec 06 §2.2), so this is a
    /// corruption signal, not an ordinary outcome.
    NotUtf8,
}

impl std::fmt::Display for SnippetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnippetError::SpanOutOfRange { span, revision_len } => write!(
                f,
                "span [{}, {}) lies outside a {revision_len}-byte revision",
                span[0], span[1]
            ),
            SnippetError::NotUtf8 => f.write_str("span bytes are not valid UTF-8"),
        }
    }
}

impl std::error::Error for SnippetError {}

/// Cut `[span.0, span.1)` out of `bytes`, capped at [`SNIPPET_CAP_BYTES`].
///
/// `bytes` is a revision's exact stored content (`local_rag_store::source_bytes`).
/// A truncated result carries spec 12 §2's `{hash, original_size}` describing the
/// **full** span.
pub fn cut(bytes: &[u8], span: [i64; 2]) -> Result<Snippet, SnippetError> {
    let out_of_range = || SnippetError::SpanOutOfRange {
        span,
        revision_len: bytes.len(),
    };
    if span[0] < 0 || span[1] < span[0] {
        return Err(out_of_range());
    }
    let start = usize::try_from(span[0]).map_err(|_| out_of_range())?;
    let end = usize::try_from(span[1]).map_err(|_| out_of_range())?;
    let full = bytes.get(start..end).ok_or_else(out_of_range)?;

    if full.len() <= SNIPPET_CAP_BYTES {
        let text = std::str::from_utf8(full).map_err(|_| SnippetError::NotUtf8)?;
        return Ok(Snippet::whole(text));
    }

    // Over the cap: move the cut back to a UTF-8 boundary. `floor_char_boundary`
    // is still unstable, so this walks back over continuation bytes (`10xxxxxx`)
    // — at most three, since no UTF-8 sequence is longer than four bytes.
    let mut cut_at = SNIPPET_CAP_BYTES;
    while cut_at > 0 && (full[cut_at] & 0b1100_0000) == 0b1000_0000 {
        cut_at -= 1;
    }
    let text = std::str::from_utf8(&full[..cut_at]).map_err(|_| SnippetError::NotUtf8)?;
    Ok(Snippet {
        text: text.to_string(),
        truncation: Some(Truncation {
            hash: truncated_excerpt(full),
            original_size: full.len() as i64,
        }),
    })
}

/// Cut a snippet for each occurrence in `metas`, reading each revision's bytes
/// **once**.
///
/// The batching is the point: ten hits in one file share one `file_revision_id`,
/// and `source_bytes` decompresses the whole revision on every call, so a
/// per-hit read would decompress the same file ten times. Order follows `metas`.
///
/// A failure to cut one occurrence (span out of range, non-UTF-8, a revision
/// that vanished) yields `None` for that entry plus a diagnostic — the response
/// keeps the hit with its metadata rather than dropping it, since the *ranking*
/// is still correct even when the excerpt cannot be produced.
pub(crate) fn cut_batch(
    conn: &Connection,
    metas: &[&OccurrenceMetadata],
) -> rusqlite::Result<(Vec<Option<Snippet>>, Vec<String>)> {
    let mut bytes_by_revision: HashMap<&str, Option<Vec<u8>>> = HashMap::new();
    for meta in metas {
        if !bytes_by_revision.contains_key(meta.file_revision_id.as_str()) {
            let bytes = source_bytes(conn, &meta.file_revision_id)?;
            bytes_by_revision.insert(meta.file_revision_id.as_str(), bytes);
        }
    }

    let mut snippets = Vec::with_capacity(metas.len());
    let mut diagnostics = Vec::new();
    for meta in metas {
        let Some(Some(bytes)) = bytes_by_revision.get(meta.file_revision_id.as_str()) else {
            // The revision a member path points at is a foreign key, so a
            // missing one means the store is inconsistent, not that the file is
            // simply gone.
            diagnostics.push(format!(
                "no stored bytes for revision {} of {}",
                meta.file_revision_id, meta.normalized_path
            ));
            snippets.push(None);
            continue;
        };
        match cut(bytes, [meta.span_start, meta.span_end]) {
            Ok(snippet) => snippets.push(Some(snippet)),
            Err(e) => {
                diagnostics.push(format!("snippet for {}: {e}", meta.normalized_path));
                snippets.push(None);
            }
        }
    }
    Ok((snippets, diagnostics))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_is_cut_exactly() {
        let bytes = b"fn main() { println!(\"hi\"); }\n";
        let snippet = cut(bytes, [3, 7]).expect("in range");
        assert_eq!(snippet.text, "main");
        assert_eq!(snippet.truncation, None);
    }

    #[test]
    fn an_empty_span_is_an_empty_snippet_not_an_error() {
        let snippet = cut(b"abc", [1, 1]).expect("empty is legal");
        assert_eq!(snippet.text, "");
        assert_eq!(snippet.truncation, None);
    }

    #[test]
    fn a_whole_file_span_is_cut_whole() {
        let bytes = "héllo wörld\n".as_bytes();
        let snippet = cut(bytes, [0, bytes.len() as i64]).expect("in range");
        assert_eq!(snippet.text, "héllo wörld\n");
    }

    #[test]
    fn a_span_past_the_end_is_out_of_range_not_a_truncated_read() {
        let err = cut(b"abc", [1, 99]).expect_err("out of range");
        assert!(matches!(
            err,
            SnippetError::SpanOutOfRange {
                revision_len: 3,
                ..
            }
        ));
        assert!(matches!(
            cut(b"abc", [-1, 2]).expect_err("negative start"),
            SnippetError::SpanOutOfRange { .. }
        ));
        assert!(matches!(
            cut(b"abc", [2, 1]).expect_err("inverted span"),
            SnippetError::SpanOutOfRange { .. }
        ));
    }

    #[test]
    fn non_utf8_span_bytes_are_refused() {
        // 0xFF is never valid UTF-8.
        assert_eq!(cut(&[0x61, 0xFF, 0x62], [0, 3]), Err(SnippetError::NotUtf8));
    }

    /// Exactly at the cap is *not* truncation — the boundary case that decides
    /// whether the metadata is present.
    #[test]
    fn a_span_exactly_at_the_cap_is_not_truncated() {
        let bytes = vec![b'a'; SNIPPET_CAP_BYTES];
        let snippet = cut(&bytes, [0, SNIPPET_CAP_BYTES as i64]).expect("in range");
        assert_eq!(snippet.text.len(), SNIPPET_CAP_BYTES);
        assert_eq!(snippet.truncation, None);
    }

    #[test]
    fn one_byte_over_the_cap_truncates_with_metadata() {
        let bytes = vec![b'a'; SNIPPET_CAP_BYTES + 1];
        let snippet = cut(&bytes, [0, bytes.len() as i64]).expect("in range");
        assert_eq!(snippet.text.len(), SNIPPET_CAP_BYTES);
        let truncation = snippet.truncation.expect("truncated");
        assert_eq!(
            truncation.original_size,
            SNIPPET_CAP_BYTES as i64 + 1,
            "original_size describes the full span, not what survived"
        );
        assert_eq!(truncation.hash.len(), 64);
        assert_eq!(
            truncation.hash,
            truncated_excerpt(&bytes),
            "the hash is over the full excerpt"
        );
    }

    /// The reason the boundary walk exists: a cap landing inside a multi-byte
    /// character must not produce invalid UTF-8 (which would fail the whole
    /// response), and must not silently drop bytes beyond that character.
    #[test]
    fn a_cap_inside_a_multibyte_character_moves_back_to_a_boundary() {
        // Fill to three bytes short of the cap, then a 4-byte emoji straddles it.
        let mut bytes = vec![b'a'; SNIPPET_CAP_BYTES - 3];
        bytes.extend_from_slice("😀".as_bytes()); // 4 bytes: cap falls inside it
        bytes.extend_from_slice(b"tail");

        let snippet = cut(&bytes, [0, bytes.len() as i64]).expect("in range");
        assert_eq!(
            snippet.text.len(),
            SNIPPET_CAP_BYTES - 3,
            "the straddling character is dropped whole, not split"
        );
        assert!(snippet.text.chars().all(|c| c == 'a'));
        assert!(snippet.truncation.is_some());
        // Round-tripping proves validity beyond the type system's own guarantee.
        assert_eq!(
            String::from_utf8(snippet.text.clone().into_bytes()).expect("valid utf-8"),
            snippet.text
        );
    }

    /// Every multi-byte width lands correctly, not just the 4-byte case.
    #[test]
    fn the_boundary_walk_handles_two_and_three_byte_characters() {
        for filler in ["é", "€", "😀"] {
            let width = filler.len();
            let mut bytes = vec![b'a'; SNIPPET_CAP_BYTES - 1];
            bytes.extend_from_slice(filler.as_bytes());
            bytes.extend_from_slice(b"more");
            let snippet = cut(&bytes, [0, bytes.len() as i64]).expect("in range");
            assert!(
                snippet.text.is_char_boundary(snippet.text.len()),
                "width {width} left a split character"
            );
            assert_eq!(
                snippet.text.len(),
                SNIPPET_CAP_BYTES - 1,
                "width {width}: the straddling character is dropped whole"
            );
        }
    }

    /// Identical excerpts hash identically, different ones do not — the only
    /// property the truncation hash promises.
    #[test]
    fn truncation_hashes_identify_the_excerpt() {
        let a = vec![b'a'; SNIPPET_CAP_BYTES + 10];
        let mut b = a.clone();
        *b.last_mut().expect("non-empty") = b'b';

        let ha = cut(&a, [0, a.len() as i64])
            .expect("in range")
            .truncation
            .expect("truncated")
            .hash;
        let ha2 = cut(&a, [0, a.len() as i64])
            .expect("in range")
            .truncation
            .expect("truncated")
            .hash;
        let hb = cut(&b, [0, b.len() as i64])
            .expect("in range")
            .truncation
            .expect("truncated")
            .hash;
        assert_eq!(ha, ha2);
        assert_ne!(
            ha, hb,
            "a difference beyond the cap is still visible in the hash"
        );
    }
}
