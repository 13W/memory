//! The path-free [`SyntaxLocator`] and its canonical serialization (spec 03
//! §2.4) `[FIXED]` field set.
//!
//! `SyntaxLocator = {language, syntax_path | local_ordinal, signature_fingerprint,
//! blob_id}` — **no path**. It is stored in `parsed_unit.syntax_locator` (a
//! content-shared row): by spec 01 §5.1 no content-shared row may carry any
//! path/context field, and "path in parsed_unit" is called out as a forbidden
//! violation. Filesystem/generation context lives only in the path-bearing
//! `OccurrenceLocator = {normalized_path, qualified_name, SyntaxLocator}`, which
//! this task does not build.
//!
//! ## Scope: shape and serialization
//!
//! The `[FIXED]` field set and the path-free property are fixed here, along with
//! the canonical serialization. The **derivation** of `syntax_path` and
//! `signature_fingerprint` from a real parse tree — once `[OPEN]` (O7) — is fixed
//! by **ADR-0002** and realized by the T04-03 adapter engine
//! ([`crate::parse::adapter`]); [`SyntaxLocatorDraft`] is the path-free,
//! blob-free value the parser produces, completed via [`SyntaxLocator::from_draft`]
//! at persistence (T04-06). The **graph** half of O7 remains `[OPEN]`.

use crate::parse::fingerprint::canonical_kv;
use crate::parse::language::SourceDialect;

/// The `syntax_path | local_ordinal` alternative of a [`SyntaxLocator`] (spec 03
/// §2.4).
///
/// The `Path` variant is a **structural route inside the parse tree**, not a
/// filesystem path; the path-free rule forbids filesystem/generation context, not
/// this. How the route or the ordinal is derived is `[OPEN]` (see module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxAnchor {
    /// A stable structural path within the parse tree (serialized `p:<path>`).
    Path(String),
    /// A positional fallback: ordinal among siblings (serialized `o:<ordinal>`).
    LocalOrdinal(u32),
}

/// A path-free syntax locator (spec 03 §2.4 `[FIXED]` field set).
///
/// Structurally incapable of holding a filesystem path — there is no such field.
/// [`serialize`](Self::serialize) emits only the allow-listed keys, and
/// [`parse`](Self::parse) rejects any path-like or unknown key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxLocator {
    /// The dialect of the unit ([`SourceDialect`]; the `lang` key) — a v0
    /// language, or a universal chunking policy (D-098).
    pub language: SourceDialect,
    /// The structural anchor ([`SyntaxAnchor`]; the `anchor` key).
    pub anchor: SyntaxAnchor,
    /// A fingerprint of the unit's signature (the `sig` key). Derivation is
    /// `[OPEN]`; carried opaquely in v0.
    pub signature_fingerprint: String,
    /// The `content_blob` id this unit's normalized text hashes to (the `blob`
    /// key).
    pub blob_id: String,
}

/// A [`SyntaxLocator`] before the `blob_id` is known — path-free **and**
/// blob-free (spec 03 §2.4, ADR-0002).
///
/// A parser adapter (T04-03+) produces this from the parse tree alone: it knows
/// the `language`, the structural `anchor`, and the `signature_fingerprint`, but
/// not the `blob_id` (that is the normalized-text identity of `source_blob[span]`,
/// derived at persistence time — T04-06). [`SyntaxLocator::from_draft`] completes
/// it once the `blob_id` is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxLocatorDraft {
    /// The dialect of the unit ([`SourceDialect`]).
    pub language: SourceDialect,
    /// The structural anchor ([`SyntaxAnchor`]).
    pub anchor: SyntaxAnchor,
    /// A fingerprint of the unit's signature (ADR-0002).
    pub signature_fingerprint: String,
}

/// A typed [`SyntaxLocator::parse`] failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocatorParseError {
    /// A forbidden filesystem/generation key appeared (spec 01 §5.1) — the locator
    /// must be path-free.
    PathFieldForbidden(String),
    /// A key outside the allow-list `{anchor, blob, lang, sig}` appeared.
    UnknownField(String),
    /// A required field was absent.
    MissingField(&'static str),
    /// A segment had no `key=value` shape.
    MalformedSegment(String),
    /// The `lang` value named neither a v0 language nor a universal dialect.
    UnknownLanguage(String),
    /// The `anchor` ordinal was not a valid `u32`.
    InvalidOrdinal(String),
    /// The `anchor` tag was neither `p:` nor `o:`.
    InvalidAnchor(String),
    /// More than one `anchor` segment appeared.
    DuplicateAnchor,
    /// No `anchor` segment appeared.
    MissingAnchor,
}

impl std::fmt::Display for LocatorParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocatorParseError::PathFieldForbidden(k) => {
                write!(f, "forbidden path-like key in syntax locator: {k}")
            }
            LocatorParseError::UnknownField(k) => write!(f, "unknown syntax-locator key: {k}"),
            LocatorParseError::MissingField(k) => write!(f, "missing syntax-locator field: {k}"),
            LocatorParseError::MalformedSegment(s) => {
                write!(f, "malformed syntax-locator segment: {s}")
            }
            LocatorParseError::UnknownLanguage(l) => {
                write!(f, "unknown language in syntax locator: {l}")
            }
            LocatorParseError::InvalidOrdinal(o) => {
                write!(f, "invalid local ordinal in syntax locator: {o}")
            }
            LocatorParseError::InvalidAnchor(a) => {
                write!(f, "invalid anchor tag in syntax locator: {a}")
            }
            LocatorParseError::DuplicateAnchor => write!(f, "duplicate anchor in syntax locator"),
            LocatorParseError::MissingAnchor => write!(f, "missing anchor in syntax locator"),
        }
    }
}

impl std::error::Error for LocatorParseError {}

/// Filesystem/generation keys that MUST NOT appear in a path-free locator (spec
/// 01 §5.1). A blocklist for a precise diagnostic; the allow-list in [`parse`]
/// (`{anchor, blob, lang, sig}`) is the structural guard.
const FORBIDDEN_PATH_KEYS: &[&str] = &[
    "path",
    "normalized_path",
    "display_path",
    "file",
    "filepath",
    "dir",
    "abs_path",
    "cwd",
];

impl SyntaxLocator {
    /// Complete a [`SyntaxLocatorDraft`] into a full locator once the `blob_id`
    /// (normalized-text identity of `source_blob[span]`) is known — the T04-06
    /// seam that joins the parser's structural output (path-free) to the store's
    /// content-blob world.
    pub fn from_draft(draft: SyntaxLocatorDraft, blob_id: String) -> Self {
        SyntaxLocator {
            language: draft.language,
            anchor: draft.anchor,
            signature_fingerprint: draft.signature_fingerprint,
            blob_id,
        }
    }

    /// Serialize to the canonical, path-free `parsed_unit.syntax_locator` string
    /// (spec 03 §2.4): sorted `key=value` over `{anchor, blob, lang, sig}` joined
    /// with `;`. The anchor is tagged `p:<syntax_path>` or `o:<ordinal>`.
    pub fn serialize(&self) -> String {
        let anchor = match &self.anchor {
            SyntaxAnchor::Path(p) => {
                // Forward contract for T04-03+: a syntax_path must be
                // delimiter-safe. We do NOT invent an escaping scheme here (the
                // derivation is `[OPEN]`); we document and assert the constraint.
                debug_assert!(
                    !p.contains(';') && !p.contains('='),
                    "syntax_path must not contain the reserved bytes ';' or '='"
                );
                format!("p:{p}")
            }
            SyntaxAnchor::LocalOrdinal(n) => format!("o:{n}"),
        };
        let mut pairs = [
            ("anchor", anchor),
            ("blob", self.blob_id.clone()),
            ("lang", self.language.as_str().to_string()),
            ("sig", self.signature_fingerprint.clone()),
        ];
        canonical_kv(&mut pairs)
    }

    /// Parse a canonical serialization, rejecting any path-like or unknown key.
    ///
    /// Two-layer path-freedom: the [`SyntaxLocator`] type cannot hold a path, and
    /// this guard refuses to decode a string that smuggles one
    /// ([`LocatorParseError::PathFieldForbidden`]).
    pub fn parse(s: &str) -> Result<SyntaxLocator, LocatorParseError> {
        let mut anchor: Option<SyntaxAnchor> = None;
        let mut blob: Option<String> = None;
        let mut lang: Option<SourceDialect> = None;
        let mut sig: Option<String> = None;

        for segment in s.split(';') {
            let (key, value) = segment
                .split_once('=')
                .ok_or_else(|| LocatorParseError::MalformedSegment(segment.to_string()))?;
            if FORBIDDEN_PATH_KEYS.contains(&key) {
                return Err(LocatorParseError::PathFieldForbidden(key.to_string()));
            }
            match key {
                "anchor" => {
                    if anchor.is_some() {
                        return Err(LocatorParseError::DuplicateAnchor);
                    }
                    anchor = Some(parse_anchor(value)?);
                }
                "blob" => blob = Some(value.to_string()),
                "lang" => {
                    lang =
                        Some(SourceDialect::from_str_value(value).ok_or_else(|| {
                            LocatorParseError::UnknownLanguage(value.to_string())
                        })?);
                }
                "sig" => sig = Some(value.to_string()),
                other => return Err(LocatorParseError::UnknownField(other.to_string())),
            }
        }

        Ok(SyntaxLocator {
            language: lang.ok_or(LocatorParseError::MissingField("lang"))?,
            anchor: anchor.ok_or(LocatorParseError::MissingAnchor)?,
            signature_fingerprint: sig.ok_or(LocatorParseError::MissingField("sig"))?,
            blob_id: blob.ok_or(LocatorParseError::MissingField("blob"))?,
        })
    }
}

/// Decode an anchor value: `o:<u32>` → [`SyntaxAnchor::LocalOrdinal`], `p:<path>`
/// → [`SyntaxAnchor::Path`].
fn parse_anchor(value: &str) -> Result<SyntaxAnchor, LocatorParseError> {
    if let Some(ordinal) = value.strip_prefix("o:") {
        let n = ordinal
            .parse::<u32>()
            .map_err(|_| LocatorParseError::InvalidOrdinal(ordinal.to_string()))?;
        Ok(SyntaxAnchor::LocalOrdinal(n))
    } else if let Some(path) = value.strip_prefix("p:") {
        Ok(SyntaxAnchor::Path(path.to_string()))
    } else {
        Err(LocatorParseError::InvalidAnchor(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::language::LanguageId;

    fn ordinal_locator() -> SyntaxLocator {
        SyntaxLocator {
            language: SourceDialect::Language(LanguageId::Rust),
            anchor: SyntaxAnchor::LocalOrdinal(3),
            signature_fingerprint: "ab12".to_string(),
            blob_id: "deadbeef".to_string(),
        }
    }

    fn path_locator() -> SyntaxLocator {
        SyntaxLocator {
            language: SourceDialect::Language(LanguageId::TypeScript),
            anchor: SyntaxAnchor::Path("module/Foo/method".to_string()),
            signature_fingerprint: "cafe".to_string(),
            blob_id: "0011".to_string(),
        }
    }

    #[test]
    fn syntax_locator_round_trips_both_anchors() {
        for locator in [ordinal_locator(), path_locator()] {
            let s = locator.serialize();
            assert_eq!(SyntaxLocator::parse(&s), Ok(locator));
        }
    }

    #[test]
    fn serialization_is_canonical_and_path_free() {
        let s = ordinal_locator().serialize();
        assert_eq!(s, "anchor=o:3;blob=deadbeef;lang=rust;sig=ab12");
        // Deterministic.
        assert_eq!(s, ordinal_locator().serialize());
        // Only allow-listed keys are emitted; no path-like key is present.
        let keys: Vec<&str> = s
            .split(';')
            .map(|kv| kv.split('=').next().unwrap())
            .collect();
        assert_eq!(keys, ["anchor", "blob", "lang", "sig"]);
        for forbidden in FORBIDDEN_PATH_KEYS {
            assert!(
                !keys.contains(forbidden),
                "emitted a forbidden key {forbidden}"
            );
        }
    }

    #[test]
    fn parse_rejects_path_like_keys() {
        // Every forbidden filesystem/generation key is refused with a precise
        // diagnostic — the locator must stay path-free (spec 01 §5.1).
        for key in FORBIDDEN_PATH_KEYS {
            let hostile = format!("anchor=o:0;blob=aa;lang=rust;sig=bb;{key}=x");
            assert_eq!(
                SyntaxLocator::parse(&hostile),
                Err(LocatorParseError::PathFieldForbidden((*key).to_string())),
                "expected rejection of path-like key {key}"
            );
        }
    }

    #[test]
    fn parse_rejects_unknown_and_malformed() {
        // Unknown (non-allow-listed, non-forbidden) key.
        assert_eq!(
            SyntaxLocator::parse("anchor=o:0;blob=aa;lang=rust;sig=bb;weird=x"),
            Err(LocatorParseError::UnknownField("weird".to_string()))
        );
        // Segment without `=`.
        assert_eq!(
            SyntaxLocator::parse("anchor=o:0;blobaa;lang=rust;sig=bb"),
            Err(LocatorParseError::MalformedSegment("blobaa".to_string()))
        );
        // Missing required fields.
        assert_eq!(
            SyntaxLocator::parse("anchor=o:0;lang=rust;sig=bb"),
            Err(LocatorParseError::MissingField("blob"))
        );
        assert_eq!(
            SyntaxLocator::parse("anchor=o:0;blob=aa;sig=bb"),
            Err(LocatorParseError::MissingField("lang"))
        );
        assert_eq!(
            SyntaxLocator::parse("anchor=o:0;blob=aa;lang=rust"),
            Err(LocatorParseError::MissingField("sig"))
        );
        // Missing anchor.
        assert_eq!(
            SyntaxLocator::parse("blob=aa;lang=rust;sig=bb"),
            Err(LocatorParseError::MissingAnchor)
        );
        // Unknown language.
        assert_eq!(
            SyntaxLocator::parse("anchor=o:0;blob=aa;lang=cobol;sig=bb"),
            Err(LocatorParseError::UnknownLanguage("cobol".to_string()))
        );
        // Duplicate anchor.
        assert_eq!(
            SyntaxLocator::parse("anchor=o:0;anchor=o:1;blob=aa;lang=rust;sig=bb"),
            Err(LocatorParseError::DuplicateAnchor)
        );
        // Non-numeric ordinal.
        assert_eq!(
            SyntaxLocator::parse("anchor=o:x;blob=aa;lang=rust;sig=bb"),
            Err(LocatorParseError::InvalidOrdinal("x".to_string()))
        );
        // Anchor tag neither `p:` nor `o:`.
        assert_eq!(
            SyntaxLocator::parse("anchor=z:1;blob=aa;lang=rust;sig=bb"),
            Err(LocatorParseError::InvalidAnchor("z:1".to_string()))
        );
    }
}
