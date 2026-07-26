//! The canonical `search_code` response and mode vocabulary (spec 09 §5/§7) —
//! T12-03.
//!
//! These types live here, not in `local-rag-search`, for the same reason
//! [`ErrorEnvelope`](crate::ErrorEnvelope) does: they are the *wire* contract
//! every caller of the MCP `search_code` tool (spec 11 §2) sees, independent of
//! which subsystem produced them.
//!
//! # Serialization
//!
//! [`SearchResponse`] derives `Serialize` in exactly spec 09 §7's shape —
//! field names, `degraded: null | "dense_only" | "lexical_only"`, and a `legs`
//! object that only carries the legs that actually matched. The shape is
//! `[SPEC]`-fixed by that section, so deriving it here implements a decision
//! rather than making one; transport, handshake framing and the MCP envelope
//! around it remain group 15's.
//!
//! Serialization is what makes T12-03's "repeated output is byte-stable"
//! testable as bytes: `serde_json` emits struct fields in declaration order, so
//! a deterministic *value* (which fusion guarantees — spec 09 §4's
//! `(score desc, occurrence_id asc)`) yields deterministic *bytes*.
//!
//! `Deserialize` is deliberately **not** derived: nothing reads these back yet,
//! and a parser with no caller is undead code.

use std::fmt;

use serde::Serialize;

use crate::error::DegradedMode;

/// Which legs a `search_code` request asks for (spec 09 §5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Lexical + dense(`code_raw`); the default (spec 09 §5).
    #[default]
    Hybrid,
    /// FTS only.
    Lexical,
    /// The dense code leg only.
    Code,
    /// The description leg — **post-v0**, gated on the benchmark `[FIXED]`.
    /// Always answered with
    /// [`ErrorCode::UnsupportedMode`](crate::ErrorCode::UnsupportedMode) until
    /// then.
    Semantic,
}

impl SearchMode {
    /// The wire-format string (spec 09 §5's `mode` column).
    pub fn as_str(self) -> &'static str {
        match self {
            SearchMode::Hybrid => "hybrid",
            SearchMode::Lexical => "lexical",
            SearchMode::Code => "code",
            SearchMode::Semantic => "semantic",
        }
    }

    /// Parse a wire value; `None` for anything spec 09 §5 does not name.
    ///
    /// `semantic` parses **successfully** — it is a real mode that is not
    /// supported yet, and the caller deserves `UNSUPPORTED_MODE` rather than
    /// "unknown mode". Only a genuinely unrecognized string is `None`.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "hybrid" => Some(SearchMode::Hybrid),
            "lexical" => Some(SearchMode::Lexical),
            "code" => Some(SearchMode::Code),
            "semantic" => Some(SearchMode::Semantic),
            _ => None,
        }
    }

    /// Whether this mode asks for the lexical (FTS) leg.
    pub fn wants_lexical(self) -> bool {
        matches!(self, SearchMode::Hybrid | SearchMode::Lexical)
    }

    /// Whether this mode asks for the dense (`code_raw`) leg.
    pub fn wants_dense(self) -> bool {
        matches!(self, SearchMode::Hybrid | SearchMode::Code)
    }
}

impl fmt::Display for SearchMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-leg rank of one result (spec 09 §7's `"legs": {"lexical": 3, "dense": 1}`).
///
/// A leg that did not match this document — or did not run at all — contributes
/// no key, which is why both fields are skipped when `None` rather than
/// serialized as `null`: "rank 0" and "no rank" would otherwise be
/// indistinguishable to a reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct LegRanks {
    /// 1-based rank in the lexical leg, if it matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lexical: Option<usize>,
    /// 1-based rank in the dense leg, if it matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dense: Option<usize>,
}

/// What a size cap cut away (spec 12 §2 `[FIXED]`: "truncation always leaves
/// `{hash, original_size}` metadata").
///
/// The hash is over the **full**, pre-truncation excerpt
/// (`local_rag_core::identity::domain::truncated_excerpt`), so a caller can tell
/// two truncated excerpts apart — and tell that a re-fetch would return the same
/// thing — without the bytes being kept anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Truncation {
    /// `H(truncated_excerpt, full excerpt bytes)`.
    pub hash: String,
    /// The full excerpt's byte length, before the cap.
    pub original_size: i64,
}

/// A `source_blob`-derived excerpt (spec 09 §7's `snippet`).
///
/// # Serialization
///
/// Spec 09 §7 shows `snippet` as a plain string, and the overwhelmingly common
/// case *is* a plain string — so an untruncated snippet serializes as exactly
/// that. Only a truncated one widens to `{"text": …, "truncation": {…}}`, which
/// is the only case that has anything more to say. Keeping the common case a
/// bare string is what makes this type compatible with §7's documented shape
/// rather than a silent change to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The excerpt text (already capped).
    pub text: String,
    /// Present only when the cap actually cut something.
    pub truncation: Option<Truncation>,
}

impl Snippet {
    /// An untruncated snippet.
    pub fn whole(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            truncation: None,
        }
    }
}

impl Serialize for Snippet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.truncation {
            None => serializer.serialize_str(&self.text),
            Some(truncation) => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("Snippet", 2)?;
                s.serialize_field("text", &self.text)?;
                s.serialize_field("truncation", truncation)?;
                s.end()
            }
        }
    }
}

/// The generation a response was served from (spec 09 §7's `"generation"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GenerationRef {
    /// The generation id (spec 03 §2.1).
    pub id: String,
    /// The per-worktree monotone `generation_number`.
    pub number: i64,
}

/// One fused search hit (spec 09 §7's `results[]` element).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResult {
    /// The matched occurrence (spec 03 §1.2).
    pub occurrence_id: String,
    /// The occurrence's normalized path within the worktree.
    pub path: String,
    /// The unit's local name, empty when the unit has none (a file/text/config
    /// unit need not).
    pub name: String,
    /// The unit's qualified name, when the grammar exposes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    /// The unit kind (`symbol`/`file`/`config_section`/`text_section`/
    /// `fallback_chunk`), as its stored token.
    pub unit_kind: String,
    /// `[start, end)` byte offsets into the file's `source_blob`.
    pub span: [i64; 2],
    /// The content blob's language.
    pub language: String,
    /// The RRF score (spec 09 §4).
    pub score: f64,
    /// Which legs matched, and at what rank.
    pub legs: LegRanks,
    /// A `source_blob`-derived, span-bounded, size-capped excerpt (T12-04).
    /// `None` only when the stored bytes could not produce one — a span outside
    /// the revision, or non-UTF-8 content in a file that classified as text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Snippet>,
}

/// The canonical `search_code` response (spec 09 §7).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchResponse {
    /// Fused hits, best first, at most the request's `limit`.
    pub results: Vec<SearchResult>,
    /// The generation served.
    pub generation: GenerationRef,
    /// `null` when every requested leg served; otherwise which leg was skipped.
    pub degraded: Option<DegradedMode>,
    /// Why the response is degraded, or anything else the caller should know
    /// (spec 02 §6: "every degraded response includes the validation reason").
    pub diagnostics: Vec<String>,
}

/// One occurrence of a file, as `get_file_context` reports it (spec 11 §2:
/// "file's occurrence list (ids, kinds, names, spans) + snippet from
/// `source_blob`").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileOccurrence {
    /// The occurrence id (spec 03 §1.2).
    pub occurrence_id: String,
    /// The unit kind, as its stored token.
    pub unit_kind: String,
    /// The unit's local name, empty when it has none.
    pub name: String,
    /// The unit's qualified name, when the grammar exposes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    /// `[start, end)` byte offsets into the file's `source_blob`.
    pub span: [i64; 2],
    /// The unit's excerpt, cut from the stored bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<Snippet>,
}

/// The `get_file_context(path)` response (spec 11 §2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileContext {
    /// The normalized path, as recorded in the active generation.
    pub path: String,
    /// The generation the answer was read from.
    pub generation: GenerationRef,
    /// The file's occurrences, ascending by span start.
    pub occurrences: Vec<FileOccurrence>,
}

/// One node of `project_overview`'s directory tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewNode {
    /// The directory path, relative to the worktree root (`""` is the root).
    pub path: String,
    /// Depth below the root: `0` for the root itself.
    pub depth: usize,
    /// Files at or below this directory (recursive).
    pub file_count: usize,
    /// Occurrences at or below this directory (recursive).
    pub occurrence_count: usize,
}

/// One aggregated import specifier (`project_overview`'s "top imports").
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportCount {
    /// The module specifier exactly as it appears in the source
    /// (`unresolved_reference.reference_text`) — unresolved, because import
    /// resolution is post-v0 (spec 09 §6).
    pub specifier: String,
    /// How many references across the generation name it.
    pub count: usize,
}

/// The `project_overview()` response (spec 11 §2): "3-level tree + entry points
/// + top imports, derived from active generation".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectOverview {
    /// The generation the answer was derived from.
    pub generation: GenerationRef,
    /// Directories folded to three levels, ascending by path.
    pub tree: Vec<OverviewNode>,
    /// Conventional entry-point files present in this generation, ascending by
    /// path (see the engine's `[SPEC]` heuristic).
    pub entry_points: Vec<String>,
    /// The most-referenced import specifiers, by descending count.
    pub top_imports: Vec<ImportCount>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SearchResponse {
        SearchResponse {
            results: vec![SearchResult {
                occurrence_id: "ab".repeat(32),
                path: "src/a.ts".to_string(),
                name: "extractImports".to_string(),
                qualified_name: Some("parser.extractImports".to_string()),
                unit_kind: "symbol".to_string(),
                span: [248, 264],
                language: "typescript".to_string(),
                score: 0.031,
                legs: LegRanks {
                    lexical: Some(3),
                    dense: Some(1),
                },
                snippet: None,
            }],
            generation: GenerationRef {
                id: "gen-1".to_string(),
                number: 41,
            },
            degraded: None,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn mode_strings_match_spec_09_section_5() {
        for mode in [
            SearchMode::Hybrid,
            SearchMode::Lexical,
            SearchMode::Code,
            SearchMode::Semantic,
        ] {
            assert_eq!(SearchMode::from_wire(mode.as_str()), Some(mode));
        }
        assert_eq!(SearchMode::from_wire("graph"), None);
        assert_eq!(SearchMode::default(), SearchMode::Hybrid);
    }

    /// `semantic` is a *recognized* mode that is unsupported — the distinction
    /// that lets the caller get `UNSUPPORTED_MODE` instead of a parse failure.
    #[test]
    fn semantic_parses_even_though_it_is_unsupported() {
        assert_eq!(
            SearchMode::from_wire("semantic"),
            Some(SearchMode::Semantic)
        );
    }

    #[test]
    fn leg_selection_matches_the_spec_09_section_5_table() {
        assert!(SearchMode::Hybrid.wants_lexical() && SearchMode::Hybrid.wants_dense());
        assert!(SearchMode::Lexical.wants_lexical() && !SearchMode::Lexical.wants_dense());
        assert!(!SearchMode::Code.wants_lexical() && SearchMode::Code.wants_dense());
        assert!(!SearchMode::Semantic.wants_lexical() && !SearchMode::Semantic.wants_dense());
    }

    #[test]
    fn response_serializes_in_the_spec_09_section_7_shape() {
        let json = serde_json::to_value(sample()).expect("serialize");
        let result = &json["results"][0];
        assert_eq!(result["path"], "src/a.ts");
        assert_eq!(result["name"], "extractImports");
        assert_eq!(result["qualified_name"], "parser.extractImports");
        assert_eq!(result["unit_kind"], "symbol");
        assert_eq!(result["span"][0], 248);
        assert_eq!(result["span"][1], 264);
        assert_eq!(result["language"], "typescript");
        assert_eq!(result["score"], 0.031);
        assert_eq!(result["legs"]["lexical"], 3);
        assert_eq!(result["legs"]["dense"], 1);
        assert_eq!(json["generation"]["number"], 41);
        assert!(json["degraded"].is_null(), "no degradation ⇒ null");
        assert_eq!(json["diagnostics"].as_array().expect("array").len(), 0);
    }

    /// A leg that did not match contributes no key at all — `"legs": {}` rather
    /// than `{"lexical": null}`, and `snippet` is absent rather than `null`
    /// until T12-04 fills it.
    #[test]
    fn absent_legs_and_snippets_are_omitted_not_nulled() {
        let mut response = sample();
        response.results[0].legs = LegRanks {
            lexical: None,
            dense: Some(2),
        };
        response.results[0].qualified_name = None;
        let json = serde_json::to_value(&response).expect("serialize");
        let result = &json["results"][0];
        assert!(result["legs"].get("lexical").is_none());
        assert_eq!(result["legs"]["dense"], 2);
        assert!(result.get("snippet").is_none());
        assert!(result.get("qualified_name").is_none());
    }

    #[test]
    fn degraded_serializes_as_its_spec_string() {
        let mut response = sample();
        response.degraded = Some(DegradedMode::LexicalOnly);
        response.diagnostics = vec!["shard unavailable".to_string()];
        let json = serde_json::to_value(&response).expect("serialize");
        assert_eq!(json["degraded"], "lexical_only");
        assert_eq!(json["diagnostics"][0], "shard unavailable");
    }

    /// Field order is declaration order, so an equal value serializes to equal
    /// bytes — the property T12-03's byte-stability requirement rests on.
    #[test]
    fn equal_responses_serialize_to_equal_bytes() {
        let a = serde_json::to_vec(&sample()).expect("serialize");
        let b = serde_json::to_vec(&sample()).expect("serialize");
        assert_eq!(a, b);
    }
}
