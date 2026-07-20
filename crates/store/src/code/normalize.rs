//! Versioned text normalization and `content_blob` identity derivation
//! (spec 03 §1.2, §2.3, §4.2) — T03-04.
//!
//! A `content_blob` row in `state.sqlite` carries **identity + metadata only**
//! (`blob_id`, `language`, `algo_version`, `normalization_version`,
//! `created_at`); the normalized text itself lives in the rebuildable
//! `normalized_text_cache` in `cache.sqlite` (spec 03 §4.2). Both are *derived*
//! from the exact `source_blob`:
//!
//! 1. **normalize** the decoded UTF-8 source into a canonical form
//!    ([`normalize`], versioned by [`NORMALIZATION_VERSION`]);
//! 2. **derive identity** `blob_id = H(content_blob: algo_version, language,
//!    normalization_version, normalized_text)` ([`content_blob_id`], spec §1.2).
//!
//! Because the identity is a hash *of the normalized text*, a cache row is valid
//! iff its stored text reproduces its `blob_id`; the cache is fully
//! reconstructible from `source_blob` (spec 06 §4). The normalized text is
//! independent of the byte offsets in `source_blob` — unit spans and snippets
//! address `source_blob` directly (spec 09 §7), so normalization may transform
//! the text freely without breaking reconstruction (the cache is always
//! recomputed from source, never inverted).
//!
//! ## Normalization v0 `[SPEC]`
//!
//! [`NORMALIZATION_VERSION`] `= 1` is a deterministic, dependency-light pipeline,
//! applied in this order:
//!
//! 1. strip a single leading UTF-8 BOM (`U+FEFF`);
//! 2. canonicalize newlines: `CRLF` and lone `CR` → `LF`;
//! 3. Unicode **NFC** (consistent with path identity, spec 03 §1.3);
//! 4. trim trailing whitespace per line (split on `\n`, `trim_end` each segment,
//!    rejoin with `\n` — preserves the line count and whether a final newline was
//!    present).
//!
//! The pipeline is idempotent. Bumping it (a new [`NORMALIZATION_VERSION`])
//! changes every `blob_id` and forces a cache rebuild — a deliberate,
//! version-gated event, never an implementation convenience.
//!
//! ## Integer field width `[SPEC]`
//!
//! `content_blob` is the first domain in the codebase to encode an integer field
//! (spec §1.2 fixes "little-endian bytes of the declared width" but leaves the
//! width to the owning task). This module declares `algo_version` and
//! `normalization_version` as **little-endian `u32`** (4 bytes) — matching the
//! `le_u32` length framing of [`hash`] and `HASH_SCHEMA_VERSION: u32`. A golden
//! test pins this. The `content_blob` DB columns stay `i64` (SQLite affinity);
//! only the hash pre-image uses `u32`.

use local_rag_core::identity::Domain;
use local_rag_core::identity::domain::hash;
use unicode_normalization::UnicodeNormalization;

/// The `content_blob` derivation-algorithm version (`content_blob.algo_version`).
///
/// Distinct from [`NORMALIZATION_VERSION`]: this versions the *derivation as a
/// whole* (how the normalized text becomes an identity), leaving room to evolve
/// the pipeline shape without re-defining what "normalized text" means. Encoded
/// as little-endian `u32` in the `blob_id` pre-image.
pub const ALGO_VERSION: u32 = 1;

/// The text-normalization version (`content_blob.normalization_version`).
///
/// Names the exact transform in [`normalize`]. Bumping it changes every derived
/// `blob_id` and invalidates the `normalized_text_cache` — a full cache rebuild
/// (spec 03 §4.4, 06 §4). Encoded as little-endian `u32` in the `blob_id`
/// pre-image.
pub const NORMALIZATION_VERSION: u32 = 1;

/// The Unicode byte-order mark (`U+FEFF`), stripped when leading (see [`normalize`]).
const BOM: char = '\u{FEFF}';

/// Normalize decoded UTF-8 source text into its canonical `normalization_version`
/// form (spec 03 §4.2). See the module docs for the exact, versioned pipeline.
///
/// Deterministic and idempotent: `normalize(normalize(x)) == normalize(x)`.
pub fn normalize(text: &str) -> String {
    // 1. Strip a single leading BOM.
    let without_bom = text.strip_prefix(BOM).unwrap_or(text);
    // 2. Canonicalize newlines: CRLF and lone CR → LF.
    let lf = to_lf(without_bom);
    // 3. Unicode NFC.
    let nfc: String = lf.nfc().collect();
    // 4. Trim trailing whitespace per line, preserving line count / final newline.
    trim_trailing_whitespace_per_line(&nfc)
}

/// Convert `CRLF` and lone `CR` to `LF` without allocating when there is no `\r`.
fn to_lf(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    // Replace CRLF first so a CRLF does not become a double LF, then any lone CR.
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Trim trailing whitespace from each `\n`-delimited segment and rejoin with `\n`.
///
/// Splitting on `\n` and rejoining preserves the segment count, so both the
/// number of lines and the presence/absence of a final newline are retained
/// (a trailing `\n` yields a final empty segment that trims to empty and rejoins
/// as a trailing `\n`).
fn trim_trailing_whitespace_per_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    out
}

/// Derive `content_blob.blob_id = H(content_blob: algo_version, language,
/// normalization_version, normalized_text)` (spec 03 §1.2).
///
/// Field order is fixed by the spec table; integer fields are little-endian `u32`
/// (see the module docs). Mirrors the [`super::source::content_hash`] pattern of
/// hashing a deterministic-ID domain through the generic [`hash`] entry point.
pub fn content_blob_id(
    algo_version: u32,
    language: &str,
    normalization_version: u32,
    normalized_text: &str,
) -> String {
    hash(
        Domain::ContentBlob,
        &[
            &algo_version.to_le_bytes(),
            language.as_bytes(),
            &normalization_version.to_le_bytes(),
            normalized_text.as_bytes(),
        ],
    )
}

/// A `content_blob`'s derived identity plus the normalized text that backs it.
///
/// The product of [`derive_content_blob`]: `blob_id`/`normalized_text` are
/// consistent by construction (`blob_id == content_blob_id(algo, language,
/// normalization, normalized_text)`). The version fields are stored back in the
/// `content_blob` columns as `i64`; `byte_size` is the UTF-8 length of
/// `normalized_text` (the `normalized_text_cache.byte_size` column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedContentBlob {
    /// The derived `blob_id` (`H(content_blob …)`).
    pub blob_id: String,
    /// The normalized text (lives in `normalized_text_cache`, never in state).
    pub normalized_text: String,
    /// UTF-8 byte length of `normalized_text`.
    pub byte_size: i64,
    /// The algorithm version that produced this identity ([`ALGO_VERSION`]).
    pub algo_version: i64,
    /// The normalization version that produced `normalized_text`
    /// ([`NORMALIZATION_VERSION`]).
    pub normalization_version: i64,
}

/// Derive the normalized text and `content_blob` identity from exact source text
/// (spec 03 §2.3, §4.2). Pure and CPU-bound — safe to run off the writer thread,
/// like [`super::source::prepare_source`].
///
/// `source_text` is the decoded UTF-8 source (guaranteed valid by the classifier,
/// spec 06 §2.1); callers reading a stored revision get it via
/// [`super::source::source_bytes`] + `str::from_utf8`.
pub fn derive_content_blob(language: &str, source_text: &str) -> DerivedContentBlob {
    let normalized_text = normalize(source_text);
    let byte_size = normalized_text.len() as i64;
    let blob_id = content_blob_id(
        ALGO_VERSION,
        language,
        NORMALIZATION_VERSION,
        &normalized_text,
    );
    DerivedContentBlob {
        blob_id,
        normalized_text,
        byte_size,
        algo_version: ALGO_VERSION as i64,
        normalization_version: NORMALIZATION_VERSION as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_and_lone_cr_become_lf() {
        assert_eq!(normalize("a\r\nb\rc\n"), "a\nb\nc\n");
        assert_eq!(normalize("x\r\r\ny"), "x\n\ny");
    }

    #[test]
    fn leading_bom_is_stripped_but_interior_feff_kept() {
        assert_eq!(normalize("\u{FEFF}code"), "code");
        // A non-leading U+FEFF (zero-width no-break space) is left intact.
        assert_eq!(normalize("a\u{FEFF}b"), "a\u{FEFF}b");
    }

    #[test]
    fn nfc_composes_decomposed_sequences() {
        // "e" + U+0301 (combining acute) → U+00E9 ("é").
        let decomposed = "e\u{0301}";
        assert_eq!(normalize(decomposed), "\u{00E9}");
        assert_eq!(normalize(decomposed).chars().count(), 1);
    }

    #[test]
    fn trailing_whitespace_trimmed_per_line_leading_kept() {
        assert_eq!(normalize("a  \n  b\t\n"), "a\n  b\n");
    }

    #[test]
    fn final_newline_and_line_count_preserved() {
        assert_eq!(normalize("a\nb\n"), "a\nb\n");
        assert_eq!(normalize("a\nb"), "a\nb");
        // A blank interior line survives (as an empty segment).
        assert_eq!(normalize("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn empty_input_normalizes_to_empty() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("\u{FEFF}"), "");
    }

    #[test]
    fn normalization_is_idempotent() {
        for input in [
            "",
            "\u{FEFF}a\r\nb  \r\n",
            "e\u{0301}\ttrailing \n",
            "line1\r\rline2\n\n",
            "no newline at all",
        ] {
            let once = normalize(input);
            assert_eq!(normalize(&once), once, "not idempotent for {input:?}");
        }
    }

    #[test]
    fn content_blob_id_is_stable_golden() {
        // Golden digest pins the field order AND the u32-LE integer width. If the
        // encoding of algo_version/normalization_version changes, this breaks.
        let id = content_blob_id(1, "rust", 1, "fn main() {}\n");
        assert_eq!(id.len(), 64);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(
            id,
            "908f1ded6a617f2df709f35fe1df42ecd63eab5f7898030d193a01563b7c2357",
        );
    }

    #[test]
    fn every_identity_field_changes_the_hash() {
        let base = content_blob_id(1, "rust", 1, "x");
        assert_ne!(base, content_blob_id(2, "rust", 1, "x")); // algo_version
        assert_ne!(base, content_blob_id(1, "ruby", 1, "x")); // language
        assert_ne!(base, content_blob_id(1, "rust", 2, "x")); // normalization_version
        assert_ne!(base, content_blob_id(1, "rust", 1, "y")); // normalized_text
    }

    #[test]
    fn integer_fields_use_u32_not_i64_width() {
        // Encoding `1` as u32 (4 bytes) must differ from encoding it as i64
        // (8 bytes). This guards the declared width against a silent change.
        use local_rag_core::identity::domain::hash;
        let as_u32 = content_blob_id(1, "rust", 1, "x");
        let as_i64 = hash(
            Domain::ContentBlob,
            &[
                &1i64.to_le_bytes(),
                "rust".as_bytes(),
                &1i64.to_le_bytes(),
                "x".as_bytes(),
            ],
        );
        assert_ne!(as_u32, as_i64);
    }

    #[test]
    fn derive_is_self_consistent() {
        let derived = derive_content_blob("rust", "\u{FEFF}fn  main() {}  \r\n");
        assert_eq!(derived.normalized_text, "fn  main() {}\n");
        assert_eq!(derived.byte_size, derived.normalized_text.len() as i64);
        assert_eq!(derived.algo_version, ALGO_VERSION as i64);
        assert_eq!(derived.normalization_version, NORMALIZATION_VERSION as i64);
        assert_eq!(
            derived.blob_id,
            content_blob_id(
                ALGO_VERSION,
                "rust",
                NORMALIZATION_VERSION,
                &derived.normalized_text
            )
        );
    }
}
