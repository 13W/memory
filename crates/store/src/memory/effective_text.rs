//! The one definition of "which text is embedded for a memory entry"
//! (ADR-0010 Decision 4) — T21-02.
//!
//! An entry's vector is found by a computed subject hash, never by a stored
//! key: `H(subject/memory_entry, memory_id, H(text))`
//! ([`subject_memory_entry`]). Three independent readers derive it — the
//! expected-key set ([`crate::subjects::memory_entry_subject_keys`]), the
//! backfill worker (`local_rag_embed::backfill`), and recall's dense leg
//! (`local_rag_memory::recall::dense`) — so all three must agree on *which*
//! text goes in. While there was only one text they trivially did. Once an
//! entry can also have an English variant (T21-01), a disagreement stops being
//! theoretical and becomes invisible: mismatched hashes simply find no vector,
//! the dense leg silently returns nothing, and no error is raised anywhere.
//! That is the exact failure D-067 already cost this project once.
//!
//! # The guarantee
//!
//! [`EffectiveText`] has no public constructor and private fields. The only
//! way to obtain one is [`decide_effective_text`], and the only way to hash a stored entry is
//! [`memory_entry_subject_hash`], which takes one. So "the effective text" has
//! exactly one definition, and hashing an arbitrary string as if it were a
//! stored entry's subject is not expressible.
//!
//! The value binds the `memory_id` **together with** its text rather than
//! carrying text alone: the pair is what a subject hash is made of, so binding
//! them also removes the neighbouring mistake of hashing one entry's id with
//! another's text.
//!
//! [`subject_memory_entry`] itself stays where its siblings and its
//! known-answer tests live (`local_rag_core::identity::domain`); a source lint
//! (`crates/store/tests/memory_subject_lint.rs`) pins it to exactly two
//! files — its definition and this module — so the raw form cannot quietly
//! grow a third caller.
//!
//! # One statement, not two reads
//!
//! [`NORMALIZATION_JOIN`]/[`NORMALIZATION_COLUMNS`] and
//! [`effective_text_from_row`] exist so every reader takes the entry and its
//! normalization in a **single** statement. That makes "new text with an old
//! translation" unobservable as a property of the SQLite snapshot rather than
//! of developer discipline: two separate reads could straddle a commit, one
//! cannot.

use rusqlite::Row;

use local_rag_core::hash::sha256_hex;
use local_rag_core::identity::domain::subject_memory_entry;

use super::normalization::NormalizationStatus;

/// The text an entry is embedded and hashed under, bound to that entry.
///
/// Produced only by [`decide_effective_text`]. The fields are private and there is no
/// constructor: a caller cannot assert that some string is an entry's
/// effective text, it can only ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveText {
    memory_id: String,
    text: String,
    normalized: bool,
}

impl EffectiveText {
    /// The entry this text belongs to.
    pub fn memory_id(&self) -> &str {
        &self.memory_id
    }

    /// The text to embed — the English variant when one is usable, otherwise
    /// the entry's own text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Whether [`as_str`](Self::as_str) is a normalization rather than the
    /// entry's own text. Nothing in the pipeline branches on this; it exists
    /// so a diagnostic surface can say *why* a hash is what it is.
    pub fn is_normalized(&self) -> bool {
        self.normalized
    }
}

/// One entry's normalization row, as the shared mapper read it.
///
/// `status` is `None` when the stored value is outside
/// [`NormalizationStatus`]'s domain — impossible through the table's own
/// CHECK, and degraded here to "no usable variant" rather than an error,
/// because a corrupt row must cost an entry its translation, never its recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizationView<'a> {
    pub status: Option<NormalizationStatus>,
    pub source_text_sha256: &'a str,
    pub normalized_text: Option<&'a str>,
}

/// Decide which text represents `memory_id` for embedding and hashing.
///
/// The English variant is used only when **all** of these hold:
///
/// - the row says [`NormalizationStatus::Ready`];
/// - its `source_text_sha256` still matches `original` — staleness is the hash
///   and never `entry_version`, because `reinforce` bumps the version without
///   touching the text;
/// - the variant is present and not blank.
///
/// Every other case yields `original`. That is deliberate and load-bearing
/// (ADR-0010 Decision 5): the original is the text whose hash the store has
/// been using all along, so a degraded case returns the system to a
/// known-good state — an existing vector — rather than to a new one with no
/// vector at all.
pub fn decide_effective_text(
    memory_id: &str,
    original: &str,
    normalization: Option<NormalizationView<'_>>,
) -> EffectiveText {
    let original_text = || EffectiveText {
        memory_id: memory_id.to_string(),
        text: original.to_string(),
        normalized: false,
    };

    let Some(view) = normalization else {
        return original_text();
    };
    if view.status != Some(NormalizationStatus::Ready) {
        return original_text();
    }
    if sha256_hex(original.as_bytes()) != view.source_text_sha256 {
        return original_text();
    }
    let Some(variant) = view.normalized_text else {
        return original_text();
    };
    if variant.trim().is_empty() {
        return original_text();
    }
    EffectiveText {
        memory_id: memory_id.to_string(),
        text: variant.to_string(),
        normalized: true,
    }
}

/// The `embedding_cache.subject_hash` of a stored memory entry — the only way
/// to compute one.
///
/// Every reader that needs to find, expect, or write an entry's vector goes
/// through here, which is what keeps the three of them byte-identical
/// (`crates/store/tests/memory_normalization_parity.rs` proves it).
pub fn memory_entry_subject_hash(text: &EffectiveText) -> String {
    subject_memory_entry(&text.memory_id, &text.text)
}

/// The join every reader of an entry's effective text appends, aliasing
/// `memory_entry` as `e` and its normalization as `n`.
pub(crate) const NORMALIZATION_JOIN: &str =
    " LEFT JOIN memory_text_normalization n ON n.memory_id = e.memory_id ";

/// The three normalization columns [`effective_text_from_row`] expects, in
/// order. Appended to a reader's own `SELECT` list.
pub(crate) const NORMALIZATION_COLUMNS: &str = "n.status, n.source_text_sha256, n.normalized_text";

/// Map [`NORMALIZATION_COLUMNS`] starting at `first_col` into the entry's
/// effective text. `NULL` in the join's key column means the entry has no
/// normalization row at all.
pub(crate) fn effective_text_from_row(
    r: &Row<'_>,
    memory_id: &str,
    original: &str,
    first_col: usize,
) -> rusqlite::Result<EffectiveText> {
    let raw_status: Option<String> = r.get(first_col)?;
    let source_text_sha256: Option<String> = r.get(first_col + 1)?;
    let normalized_text: Option<String> = r.get(first_col + 2)?;

    let view = match (raw_status, source_text_sha256) {
        (Some(raw_status), Some(source_text_sha256)) => Some((
            NormalizationStatus::from_db(&raw_status),
            source_text_sha256,
            normalized_text,
        )),
        // No row (or a row whose NOT NULL columns are somehow absent): the
        // effective text is the original, which is what the store did before
        // this table existed.
        _ => None,
    };
    Ok(decide_effective_text(
        memory_id,
        original,
        view.as_ref().map(|(status, sha, text)| NormalizationView {
            status: *status,
            source_text_sha256: sha,
            normalized_text: text.as_deref(),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "исходный текст";
    const VARIANT: &str = "source text";

    fn view<'a>(
        status: Option<NormalizationStatus>,
        sha: &'a str,
        text: Option<&'a str>,
    ) -> NormalizationView<'a> {
        NormalizationView {
            status,
            source_text_sha256: sha,
            normalized_text: text,
        }
    }

    /// Every case the decision has, in one table: exactly one of them yields
    /// the variant, and every other yields the original — the property the
    /// whole fallback design rests on.
    #[test]
    fn decide_uses_the_variant_only_when_everything_lines_up() {
        let current = sha256_hex(ORIGINAL.as_bytes());
        let stale = sha256_hex("что-то другое".as_bytes());

        let cases: Vec<(&str, Option<NormalizationView<'_>>, &str, bool)> = vec![
            ("no normalization row at all", None, ORIGINAL, false),
            (
                "ready, current hash, real variant",
                Some(view(
                    Some(NormalizationStatus::Ready),
                    &current,
                    Some(VARIANT),
                )),
                VARIANT,
                true,
            ),
            (
                "ready, but the text moved under it",
                Some(view(
                    Some(NormalizationStatus::Ready),
                    &stale,
                    Some(VARIANT),
                )),
                ORIGINAL,
                false,
            ),
            (
                "skipped: already in the target script",
                Some(view(Some(NormalizationStatus::Skipped), &current, None)),
                ORIGINAL,
                false,
            ),
            (
                "failed: no usable variant was produced",
                Some(view(Some(NormalizationStatus::Failed), &current, None)),
                ORIGINAL,
                false,
            ),
            (
                "ready, but the variant is only whitespace",
                Some(view(
                    Some(NormalizationStatus::Ready),
                    &current,
                    Some("   \n\t "),
                )),
                ORIGINAL,
                false,
            ),
            (
                "ready with no variant at all (the CHECK forbids it; degrade anyway)",
                Some(view(Some(NormalizationStatus::Ready), &current, None)),
                ORIGINAL,
                false,
            ),
            (
                "a status outside the domain (corrupt row)",
                Some(view(None, &current, Some(VARIANT))),
                ORIGINAL,
                false,
            ),
        ];

        for (name, normalization, expected_text, expected_normalized) in cases {
            let effective = decide_effective_text("m-1", ORIGINAL, normalization);
            assert_eq!(effective.as_str(), expected_text, "case: {name}");
            assert_eq!(
                effective.is_normalized(),
                expected_normalized,
                "case: {name}"
            );
            assert_eq!(effective.memory_id(), "m-1", "case: {name}");
        }
    }

    /// The hash is a function of the pair, and of nothing else: the same text
    /// under two entries gives two subjects, and a normalized entry's subject
    /// is the subject of its *variant*.
    #[test]
    fn the_subject_hash_follows_the_effective_text_and_its_entry() {
        let current = sha256_hex(ORIGINAL.as_bytes());
        let plain_a = decide_effective_text("m-a", ORIGINAL, None);
        let plain_b = decide_effective_text("m-b", ORIGINAL, None);
        assert_ne!(
            memory_entry_subject_hash(&plain_a),
            memory_entry_subject_hash(&plain_b),
            "the same text under two entries must never share a subject",
        );

        let normalized = decide_effective_text(
            "m-a",
            ORIGINAL,
            Some(view(
                Some(NormalizationStatus::Ready),
                &current,
                Some(VARIANT),
            )),
        );
        assert_ne!(
            memory_entry_subject_hash(&plain_a),
            memory_entry_subject_hash(&normalized),
            "normalizing an entry moves its subject — that is why the vector is \
             written under the new hash first (ADR-0010 Decision 6)",
        );
        assert_eq!(
            memory_entry_subject_hash(&normalized),
            memory_entry_subject_hash(&decide_effective_text("m-a", VARIANT, None)),
            "the subject depends only on the pair (memory_id, effective text)",
        );
    }
}
