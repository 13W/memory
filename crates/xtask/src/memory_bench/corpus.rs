//! Loads the memory-router fixture cases (spec 08 §7, 14 §1 item 4) —
//! T14-07.
//!
//! # Where the cases live (GAP-04)
//!
//! `fixtures/schema/manifest.schema.json`'s `families` array is fixed-size
//! (exactly six, closed enum) — adding a seventh top-level family for router
//! ops is schema-illegal without a schema change. GAP-04 already scopes this
//! exact corpus under the existing `"memory"` family (`fixtures/manifest.json`:
//! `{"id": "GAP-04", "family": "memory", ..., "resolves_in": "T14-07"}`), and
//! `fixtures/memory/index.json` already uses the generic case-index shape
//! (`fixtures/schema/case-index.schema.json`) for everything else
//! memory-related. The router-op cases this module loads are therefore new
//! cases *within* `fixtures/memory/index.json`, distinguished from every
//! other case in that file (including the older, differently-shaped
//! `memory.router.extract-valid-ops` case — v1's status-based op vocabulary,
//! not this benchmark's `GeneratedOp`-based one) purely by an id prefix:
//! [`CASE_ID_PREFIX`]. This is a deliberately narrower, less disruptive
//! choice than the original plan's "new family `fixtures/router/`" — see the
//! commit history for the design's evolution once the schema's closed enum
//! was discovered.
//!
//! # Case shape (`input`/`expected`, both untyped in the shared schema)
//!
//! ```json
//! {
//!   "id": "memory.router.op.create-decision-en-clean",
//!   "title": "...",
//!   "status": "active",
//!   "tags": ["router", "create", "en", "clean"],
//!   "provenance": { "source": "T14-07 fixture authoring", "note": "..." },
//!   "input": {
//!     "existing_entries": [
//!       { "ref": "e1", "kind": "convention", "scope_kind": "global",
//!         "canonical_key": "storage-backend", "text": "..." }
//!     ],
//!     "observations": [
//!       { "id": "o1", "event_type": "UserPromptSubmit",
//!         "evidence_kind": "user_statement", "trust": "normal", "text": "..." }
//!     ]
//!   },
//!   "expected": { "op_kinds": ["create"] }
//! }
//! ```
//!
//! Every case is deliberately `scope_kind: "global"` — `crate::memory_bench`
//! tests router *quality* (explicit-durable vs not, evidence-trust, RU/EN),
//! not scope-owner resolution, which `local_rag_memory::guard`'s own mocked
//! unit tests (T14-07 Phase 3c) already cover directly. `existing_entries`
//! are seeded into a throwaway `memory_entry` table before each case runs;
//! their real (harness-minted) `memory_id` is what the model sees in its own
//! prompt (`local_rag_memory::recall::candidate_conflict_set`) and is
//! expected to echo back for a `reinforce`/`resolve`/`retract`/`supersede`
//! case — `ref` exists only for a human reader's benefit, never consumed by
//! the harness itself.

use serde::Deserialize;

/// The prefix that marks a `fixtures/memory/index.json` case as one of this
/// benchmark's own (see the module doc).
pub const CASE_ID_PREFIX: &str = "memory.router.op.";

#[derive(Debug, Clone, Deserialize)]
struct CaseIndexFile {
    family: String,
    version: String,
    cases: Vec<RawCase>,
}

/// `input`/`expected` stay untyped here: `fixtures/memory/index.json` holds
/// many cases this module does not own (arbitrary, differently-shaped JSON
/// per the case-index schema), so every case must parse at this level
/// regardless of its own `input`/`expected` shape. Only a case that passes
/// the [`CASE_ID_PREFIX`]/`status` filter is subsequently parsed into
/// [`CaseInput`]/[`CaseExpected`] — see [`select_router_cases`].
#[derive(Debug, Clone, Deserialize)]
struct RawCase {
    id: String,
    #[allow(dead_code)]
    title: String,
    status: String,
    #[serde(default)]
    tags: Vec<String>,
    input: serde_json::Value,
    expected: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaseInput {
    #[serde(default)]
    pub existing_entries: Vec<CaseExistingEntry>,
    pub observations: Vec<CaseObservation>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaseExistingEntry {
    #[serde(rename = "ref")]
    #[allow(dead_code)]
    pub reference: String,
    pub kind: String,
    pub scope_kind: String,
    pub text: String,
    #[serde(default)]
    pub canonical_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaseObservation {
    pub id: String,
    pub event_type: String,
    pub evidence_kind: String,
    #[serde(default = "default_trust")]
    pub trust: String,
    pub text: String,
}

fn default_trust() -> String {
    "normal".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaseExpected {
    pub op_kinds: Vec<String>,
}

/// One loaded, ready-to-run router-op case.
#[derive(Debug, Clone)]
pub struct RouterCase {
    pub id: String,
    pub tags: Vec<String>,
    pub input: CaseInput,
    pub expected: CaseExpected,
}

/// Why loading the corpus failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorpusError {
    Load(String),
    UnexpectedFamily(String),
    DuplicateId(String),
    /// A case's `expected.op_kinds` names something outside
    /// [`crate::memory_bench::score::CLASSES`] — almost always a fixture typo,
    /// caught here rather than silently scoring as a permanent miss.
    UnknownOpKind {
        case_id: String,
        kind: String,
    },
    EmptyCorpus,
}

impl std::fmt::Display for CorpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CorpusError::Load(detail) => write!(f, "loading the case index: {detail}"),
            CorpusError::UnexpectedFamily(family) => {
                write!(f, "expected family \"memory\", got {family:?}")
            }
            CorpusError::DuplicateId(id) => write!(f, "duplicate case id {id:?}"),
            CorpusError::UnknownOpKind { case_id, kind } => write!(
                f,
                "{case_id}: expected.op_kinds names {kind:?}, which is not one of {:?}",
                crate::memory_bench::score::CLASSES
            ),
            CorpusError::EmptyCorpus => write!(
                f,
                "no {CASE_ID_PREFIX}* cases found in the case index -- nothing to score"
            ),
        }
    }
}

impl std::error::Error for CorpusError {}

/// Load every `active`, [`CASE_ID_PREFIX`]-prefixed case from the
/// `fixtures/memory/index.json` case index at `path`. Returns the corpus
/// `version` string alongside the cases (for [`crate::memory_bench::report::Provenance`]).
/// `deferred` cases and every non-router-op case already in that file are
/// silently skipped, not errors — this file has always held cases this
/// module does not own.
pub fn load_router_cases(path: &std::path::Path) -> Result<(String, Vec<RouterCase>), CorpusError> {
    let text =
        std::fs::read_to_string(path).map_err(|e| CorpusError::Load(format!("{path:?}: {e}")))?;
    let file: CaseIndexFile =
        serde_json::from_str(&text).map_err(|e| CorpusError::Load(e.to_string()))?;
    select_router_cases(file)
}

/// The pure part of [`load_router_cases`] (no file I/O), split out so tests
/// can exercise every rejection path against an in-memory
/// [`CaseIndexFile`] instead of writing fixture files to disk (mirrors
/// `crate::bench::corpus::Corpus::validate`'s own load/validate split).
fn select_router_cases(file: CaseIndexFile) -> Result<(String, Vec<RouterCase>), CorpusError> {
    if file.family != "memory" {
        return Err(CorpusError::UnexpectedFamily(file.family));
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut cases = Vec::new();
    for raw in file.cases {
        if !raw.id.starts_with(CASE_ID_PREFIX) {
            continue;
        }
        if raw.status != "active" {
            continue;
        }
        if !seen.insert(raw.id.clone()) {
            return Err(CorpusError::DuplicateId(raw.id));
        }
        let input: CaseInput = serde_json::from_value(raw.input).map_err(|e| {
            CorpusError::Load(format!(
                "{}: input does not match the router-op shape: {e}",
                raw.id
            ))
        })?;
        let expected: CaseExpected = serde_json::from_value(raw.expected).map_err(|e| {
            CorpusError::Load(format!(
                "{}: expected does not match the router-op shape: {e}",
                raw.id
            ))
        })?;
        for kind in &expected.op_kinds {
            if !crate::memory_bench::score::CLASSES.contains(&kind.as_str()) {
                return Err(CorpusError::UnknownOpKind {
                    case_id: raw.id,
                    kind: kind.clone(),
                });
            }
        }
        cases.push(RouterCase {
            id: raw.id,
            tags: raw.tags,
            input,
            expected,
        });
    }

    if cases.is_empty() {
        return Err(CorpusError::EmptyCorpus);
    }

    Ok((file.version, cases))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_case(id: &str, status: &str) -> String {
        format!(
            r#"{{
                "id": "{id}",
                "title": "t",
                "status": "{status}",
                "provenance": {{"source": "test"}},
                "input": {{"observations": [
                    {{"id": "o1", "event_type": "UserPromptSubmit", "evidence_kind": "user_statement", "text": "we decided X"}}
                ]}},
                "expected": {{"op_kinds": ["create"]}}
            }}"#
        )
    }

    fn parse(json: &str) -> CaseIndexFile {
        serde_json::from_str(json).expect("valid case-index JSON")
    }

    #[test]
    fn loads_only_active_router_op_prefixed_cases() {
        let json = format!(
            r#"{{
                "family": "memory",
                "version": "1.0.0",
                "cases": [
                    {},
                    {},
                    {{
                        "id": "memory.router.extract-valid-ops",
                        "title": "unrelated v1 case",
                        "status": "active",
                        "provenance": {{"source": "test"}},
                        "input": {{"observations": []}},
                        "expected": {{"op_kinds": []}}
                    }}
                ]
            }}"#,
            minimal_case("memory.router.op.a", "active"),
            minimal_case("memory.router.op.b", "deferred"),
        );

        let (version, cases) = select_router_cases(parse(&json)).expect("loads");
        assert_eq!(version, "1.0.0");
        assert_eq!(
            cases.len(),
            1,
            "deferred and non-prefixed cases are skipped"
        );
        assert_eq!(cases[0].id, "memory.router.op.a");
    }

    #[test]
    fn a_duplicate_id_is_rejected() {
        let json = format!(
            r#"{{"family": "memory", "version": "1.0.0", "cases": [{}, {}]}}"#,
            minimal_case("memory.router.op.a", "active"),
            minimal_case("memory.router.op.a", "active"),
        );
        assert_eq!(
            select_router_cases(parse(&json)).unwrap_err(),
            CorpusError::DuplicateId("memory.router.op.a".to_string())
        );
    }

    #[test]
    fn an_unexpected_family_is_rejected() {
        let json = format!(
            r#"{{"family": "search", "version": "1.0.0", "cases": [{}]}}"#,
            minimal_case("memory.router.op.a", "active"),
        );
        assert_eq!(
            select_router_cases(parse(&json)).unwrap_err(),
            CorpusError::UnexpectedFamily("search".to_string())
        );
    }

    #[test]
    fn an_op_kind_outside_the_declared_vocabulary_is_rejected() {
        let json = r#"{
            "family": "memory",
            "version": "1.0.0",
            "cases": [{
                "id": "memory.router.op.a",
                "title": "t",
                "status": "active",
                "provenance": {"source": "test"},
                "input": {"observations": [
                    {"id": "o1", "event_type": "UserPromptSubmit", "evidence_kind": "user_statement", "text": "x"}
                ]},
                "expected": {"op_kinds": ["not-a-real-op"]}
            }]
        }"#;
        assert_eq!(
            select_router_cases(parse(json)).unwrap_err(),
            CorpusError::UnknownOpKind {
                case_id: "memory.router.op.a".to_string(),
                kind: "not-a-real-op".to_string(),
            }
        );
    }

    #[test]
    fn an_empty_result_is_rejected_not_silently_scored_as_perfect() {
        let json = r#"{"family": "memory", "version": "1.0.0", "cases": []}"#;
        assert_eq!(
            select_router_cases(parse(json)).unwrap_err(),
            CorpusError::EmptyCorpus
        );
    }

    #[test]
    fn the_shipped_fixture_loads_and_every_case_has_a_non_empty_op_kinds_list() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/memory/index.json");
        let (_version, cases) = load_router_cases(&path).expect("shipped fixture loads");
        assert!(
            cases.len() >= 30,
            "expected a substantial corpus, got {}",
            cases.len()
        );
        for case in &cases {
            assert!(
                !case.expected.op_kinds.is_empty(),
                "{}: expected.op_kinds must not be empty",
                case.id
            );
            assert!(
                !case.input.observations.is_empty(),
                "{}: input.observations must not be empty",
                case.id
            );
        }
    }

    /// D-080: at least one case must put more entries in front of the router
    /// than `MAX_PROMPT_CANDIDATES` allows through, or the corpus cannot
    /// observe the selection rule at all. Before D-080 the whole corpus
    /// seeded at most **one** existing entry against a cap of 50, so the
    /// router being blind to everything recent was invisible here — the
    /// defect had to be found on a live store instead.
    #[test]
    fn the_shipped_fixture_has_a_case_that_saturates_the_prompt_candidate_cap() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/memory/index.json");
        let (_version, cases) = load_router_cases(&path).expect("shipped fixture loads");
        let cap = local_rag_memory::recall::MAX_PROMPT_CANDIDATES;
        let deepest = cases
            .iter()
            .map(|c| c.input.existing_entries.len())
            .max()
            .unwrap_or(0);
        assert!(
            deepest > cap,
            "no case seeds more than the {cap}-entry prompt cap (deepest is {deepest}), so the \
             corpus cannot tell a working candidate selection from a broken one"
        );
    }

    #[test]
    fn the_shipped_fixture_has_both_ru_and_en_cases() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/memory/index.json");
        let (_version, cases) = load_router_cases(&path).expect("shipped fixture loads");
        assert!(cases.iter().any(|c| c.tags.iter().any(|t| t == "en")));
        assert!(cases.iter().any(|c| c.tags.iter().any(|t| t == "ru")));
    }
}
