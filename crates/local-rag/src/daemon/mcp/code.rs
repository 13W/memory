//! The three MCP code-query tool adapters: build a `SearchRequest`/
//! `RequestRoot`, call [`SearchEngine`], map its double `Result` into a
//! [`CallToolResult`] — T15-03.

use std::path::Path;

use serde_json::{Map, Value};

use local_rag_core::identity::path::{
    CaseSensitivity, canonicalize_absolute, normalize_absolute_str, normalize_relative,
};
use local_rag_protocol::{ErrorEnvelope, SearchMode};
use local_rag_search::{SearchEngine, SearchInfraError, SearchRequest};
use local_rag_store::{RequestRoot, WorktreeRootFacts};

use crate::daemon::gitroot;
use crate::daemon::normalization::boundary::Translator;

use super::content::{self, CallToolResult};
use super::tools::{DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT, reject_unknown_keys, require_string};

/// Fold the engine's `Result<Result<T, ErrorEnvelope>, SearchInfraError>`
/// into one `CallToolResult` — every one of the three outcomes (success,
/// domain failure, infra failure) is a normal MCP tool result, never a
/// JSON-RPC-level error (see `dispatch`'s module doc).
fn fold<T: serde::Serialize>(
    outcome: Result<Result<T, ErrorEnvelope>, SearchInfraError>,
) -> CallToolResult {
    match outcome {
        Ok(Ok(value)) => content::ok(&value),
        Ok(Err(envelope)) => content::err(&envelope),
        Err(infra) => content::infra_err(infra),
    }
}

pub async fn search_code(
    engine: &SearchEngine,
    translator: &Translator,
    root: RequestRoot,
    args: &Map<String, Value>,
    now_ms: i64,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["query", "mode", "limit", "name_pattern"])?;
    let query = require_string(args, "query")?;

    let mode = match args.get("mode") {
        None => SearchMode::default(),
        Some(Value::String(s)) => SearchMode::from_wire(s).ok_or_else(|| {
            format!("mode must be one of hybrid/lexical/code/semantic, got {s:?}")
        })?,
        Some(_) => return Err("mode must be a string".to_string()),
    };

    let limit = match args.get("limit") {
        None => DEFAULT_SEARCH_LIMIT,
        Some(Value::Number(n)) => n
            .as_i64()
            .filter(|v| (1..=MAX_SEARCH_LIMIT).contains(v))
            .ok_or_else(|| format!("limit must be an integer between 1 and {MAX_SEARCH_LIMIT}"))?,
        Some(_) => return Err("limit must be an integer".to_string()),
    };

    let name_pattern = match args.get("name_pattern") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err("name_pattern must be a string".to_string()),
    };

    // T21-19, ADR-0011 §Decision 2: the query crosses the same boundary the
    // memory one does. It matters more here, not less — indexed code is stored
    // verbatim (§Decision 5), so a non-Latin query never produces a lexical
    // match at all and reaches the dense leg alone.
    //
    // The common case costs nothing, and structurally rather than by luck: the
    // detector strips identifiers, paths and hex-like tokens before counting,
    // so an identifier-shaped query is classified English and never reaches the
    // generator.
    let (query, query_degraded) = translator.decide_query(&query).await;
    let request = SearchRequest {
        root,
        query,
        mode,
        limit: limit as usize,
        name_pattern,
        query_degraded: query_degraded.as_ref().map(|why| why.label()),
    };
    Ok(fold(engine.search_code(request, now_ms).await))
}

pub async fn get_file_context(
    engine: &SearchEngine,
    root: RequestRoot,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &["path"])?;
    let raw_path = require_string(args, "path")?;

    let case = gitroot::case_sensitivity();
    let normalized = match normalized_relative_path(&root, &raw_path, case) {
        Ok(p) => p,
        Err(envelope) => return Ok(content::err(&envelope)),
    };
    Ok(fold(engine.get_file_context(&root, &normalized).await))
}

pub async fn project_overview(
    engine: &SearchEngine,
    root: RequestRoot,
    args: &Map<String, Value>,
) -> Result<CallToolResult, String> {
    reject_unknown_keys(args, &[])?;
    // Not routed through `fold`: it returns `Arc<ProjectOverview>` (cached
    // per generation, `crates/search/src/overview.rs`), and this crate's
    // `serde` dependency doesn't enable the `rc` feature — `Arc<T>: Serialize`
    // needs it. `content::ok` only needs a borrow, so no clone is needed.
    Ok(match engine.project_overview(&root).await {
        Ok(Ok(overview)) => content::ok(overview.as_ref()),
        Ok(Err(envelope)) => content::err(&envelope),
        Err(infra) => content::infra_err(infra),
    })
}

/// Turn a caller-supplied `path` argument (relative or absolute) into the
/// worktree-relative, normalized form `SearchEngine::get_file_context`
/// expects. An absolute path outside the request's worktree root is
/// `PATH_NOT_INDEXED` — a domain answer (the string is well-formed, it
/// simply names nothing in this worktree), never a `-32602` argument error.
///
/// Never requires the final path component itself to exist — a file the
/// index knows about may have been deleted or moved since indexing, and
/// this daemon's own server instructions already promise excerpts
/// "describe what was indexed even if the file has since changed." But its
/// *parent directory*, when it still exists, is symlink-resolved the same
/// way the worktree root itself was at git-probe time
/// ([`canonicalize_absolute`]) — without that, a prefix compare against the
/// worktree root would spuriously fail on any platform where a common
/// ancestor is itself a symlink (macOS's `/tmp` → `/private/tmp`, `/var` →
/// `/private/var` are the everyday case). Only when even the parent is gone
/// does this fall back to pure string normalization
/// ([`normalize_absolute_str`]) against the raw input.
fn normalized_relative_path(
    root: &RequestRoot,
    raw: &str,
    case: CaseSensitivity,
) -> Result<String, ErrorEnvelope> {
    let candidate = Path::new(raw);
    if !candidate.is_absolute() {
        return Ok(normalize_relative(raw, case).canonical);
    }

    let Some(facts) = &root.worktree_root else {
        return Err(ErrorEnvelope::path_not_indexed(
            raw,
            "no worktree context for this request",
        ));
    };
    let normalized_input = normalize_absolute_input(candidate, case);
    let relative = Path::new(&normalized_input)
        .strip_prefix(worktree_root_path(facts))
        .map_err(|_| {
            ErrorEnvelope::path_not_indexed(raw, "path is outside the request's worktree root")
        })?;
    Ok(normalize_relative(&relative.to_string_lossy(), case).canonical)
}

/// See [`normalized_relative_path`]'s own doc for why this resolves the
/// parent directory through the filesystem (symlinks and all) but never
/// requires the final component to exist.
fn normalize_absolute_input(path: &Path, case: CaseSensitivity) -> String {
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return normalize_absolute_str(&path.to_string_lossy(), case);
    };
    match canonicalize_absolute(parent, case) {
        Ok(canonical_parent) => {
            format!(
                "{}/{}",
                canonical_parent.canonical,
                file_name.to_string_lossy()
            )
        }
        Err(_) => normalize_absolute_str(&path.to_string_lossy(), case),
    }
}

fn worktree_root_path(facts: &WorktreeRootFacts) -> &Path {
    Path::new(&facts.observed_canonical_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(canonical: &str) -> WorktreeRootFacts {
        WorktreeRootFacts {
            observed_canonical_path: canonical.to_string(),
            display_path: canonical.to_string(),
            path_fingerprint: "fp".to_string(),
            kind: local_rag_store::WorktreeKind::Main,
            common_dir_fingerprint: None,
            remote_fingerprint: None,
        }
    }

    #[test]
    fn a_relative_path_is_normalized_directly() {
        let root = RequestRoot {
            worktree_root: None,
            repo_hint: None,
        };
        let normalized =
            normalized_relative_path(&root, "src/a.rs", CaseSensitivity::Sensitive).unwrap();
        assert_eq!(normalized, "src/a.rs");
    }

    /// T16-04 (spec 12 threat model, "symlink/path tricks"): a relative
    /// `..`-traversal string is not a real vulnerability, only a coverage
    /// gap next to the existing absolute-path tests below. This function's
    /// own doc explains why: for a non-absolute `raw`, resolution never
    /// touches the filesystem or `root` at all —
    /// `normalize_relative`/`normalize_separators_and_dots` filters only
    /// empty and literal `.` segments, leaving `..` untouched — and the
    /// caller (`SearchEngine::get_file_context`,
    /// `crates/search/src/context.rs`) resolves the result via a DB lookup
    /// keyed by `(generation_id, normalized_path)`, never a filesystem
    /// read. No indexer ever writes a `normalized_path` containing `..`, so
    /// this string can only ever miss and land on the ordinary
    /// `PATH_NOT_INDEXED` answer (`adversarial.code.
    /// relative-traversal-is-inert`).
    #[test]
    fn a_relative_dot_dot_traversal_string_is_left_literal_not_resolved() {
        let root = RequestRoot {
            worktree_root: None,
            repo_hint: None,
        };
        let normalized =
            normalized_relative_path(&root, "../../etc/passwd", CaseSensitivity::Sensitive)
                .unwrap();
        assert_eq!(normalized, "../../etc/passwd");
    }

    #[test]
    fn an_absolute_path_inside_the_worktree_is_stripped_to_relative() {
        let dir = std::env::temp_dir().join(format!("local-rag-code-test-{}", std::process::id()));
        // The file's *parent* directory exists (so its symlinks resolve the
        // same way the worktree root's do — see this platform's own
        // /var -> /private/var), but the file itself never does.
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let root = RequestRoot {
            worktree_root: Some(facts(
                &canonicalize_absolute(&dir, CaseSensitivity::Sensitive)
                    .unwrap()
                    .canonical,
            )),
            repo_hint: None,
        };
        // `src/a.rs` is never created on disk: resolution must not require
        // the queried file to currently exist — a file the index knows
        // about may have been deleted or moved since indexing (see this
        // function's own doc comment).
        let absolute = dir.join("src").join("a.rs");
        let raw = absolute.to_string_lossy().into_owned();
        let normalized = normalized_relative_path(&root, &raw, CaseSensitivity::Sensitive).unwrap();
        assert_eq!(normalized, "src/a.rs");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absolute_path_outside_the_worktree_is_path_not_indexed() {
        let dir = std::env::temp_dir().join(format!(
            "local-rag-code-test-outside-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = RequestRoot {
            worktree_root: Some(facts(
                &canonicalize_absolute(&dir, CaseSensitivity::Sensitive)
                    .unwrap()
                    .canonical,
            )),
            repo_hint: None,
        };
        let err =
            normalized_relative_path(&root, "/definitely/not/inside", CaseSensitivity::Sensitive)
                .unwrap_err();
        assert_eq!(err.code, local_rag_protocol::ErrorCode::PathNotIndexed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absolute_path_with_no_worktree_context_is_path_not_indexed() {
        let root = RequestRoot {
            worktree_root: None,
            repo_hint: None,
        };
        let err =
            normalized_relative_path(&root, "/some/absolute/path", CaseSensitivity::Sensitive)
                .unwrap_err();
        assert_eq!(err.code, local_rag_protocol::ErrorCode::PathNotIndexed);
    }
}

#[cfg(test)]
mod query_boundary_tests {
    //! The code pillar's query boundary (T21-19).
    //!
    //! These exercise the decision, not the search: whether the translator is
    //! reached at all is the whole question here, because indexed code is
    //! stored verbatim and a query that never becomes English reaches only the
    //! dense leg. Running a full hybrid search to observe that would test the
    //! search engine, which `crates/search`'s own suites already do.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use local_rag_core::config::DataPolicy;
    use local_rag_embed::{
        FinishReason, GenError, GenRequest, GenResponse, Generator, GeneratorEntry, GeneratorPool,
    };
    use serde_json::json;

    use crate::daemon::normalization::boundary::Translator;

    /// The card's own claim about identifier-shaped queries, as a fixture set:
    /// the first three must never reach the generator, the last two must.
    const FREE: [&str; 3] = [
        "register_embedder_representation",
        "crates/store/src/memory/normalization.rs",
        "where is the consolidation runner's dead-letter handled",
    ];
    /// Long enough to clear the validator's `MIN_LENGTH_RATIO` against both
    /// [`PAID`] queries.
    const TRANSLATION: &str = "where is register_embedder_representation called from";

    const PAID: [&str; 2] = [
        "где вызывается register_embedder_representation",
        "покажи где демон переживает падение консолидации",
    ];

    #[derive(Clone)]
    struct CountingGenerator {
        calls: Arc<AtomicUsize>,
    }

    impl CountingGenerator {
        fn new() -> Self {
            CountingGenerator {
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn translator(&self) -> Translator {
            Translator {
                generators: Some(Arc::new(GeneratorPool::new(vec![GeneratorEntry::local(
                    "counting",
                    Arc::new(self.clone()),
                )]))),
                model_id: "counting-model".to_string(),
                policy: DataPolicy::LocalOnly,
            }
        }
    }

    impl Generator for CountingGenerator {
        fn generate(&self, _req: GenRequest) -> Result<GenResponse, GenError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Proportionate on purpose: the validator rejects an answer
            // shorter than `MIN_LENGTH_RATIO` of its source (a fragment is not
            // a translation), so a canned "translated" would be refused and
            // this test would prove the opposite of what it means to.
            Ok(GenResponse {
                text: json!({ "en": TRANSLATION }).to_string(),
                finish_reason: FinishReason::Stop,
                tokens_generated: None,
            })
        }
    }

    /// The card claims an identifier-shaped query costs nothing. That claim
    /// rests on the detector stripping identifiers, paths and hex-like tokens
    /// *before* it counts, so it is worth checking rather than repeating.
    #[tokio::test]
    async fn identifier_and_english_queries_never_reach_the_generator() {
        for query in FREE {
            let generator = CountingGenerator::new();
            let (decided, why) = generator.translator().decide_query(query).await;
            assert_eq!(
                generator.calls(),
                0,
                "{query:?} must be answered by the pure detector",
            );
            assert_eq!(decided, query, "an untranslated query is passed through");
            assert!(why.is_none());
        }
    }

    /// The other half of the same threshold, and the reason the test above is
    /// not merely proof that the detector is asleep: Russian prose still counts
    /// as Russian when an identifier is sitting inside it.
    #[tokio::test]
    async fn russian_prose_around_an_identifier_is_still_translated() {
        for query in PAID {
            let generator = CountingGenerator::new();
            let (decided, why) = generator.translator().decide_query(query).await;
            assert_eq!(
                generator.calls(),
                1,
                "{query:?} is prose in another script and must be translated",
            );
            assert_eq!(decided, TRANSLATION);
            assert!(why.is_none());
        }
    }

    /// An empty query has nothing to decide and must stay free.
    #[tokio::test]
    async fn an_empty_query_never_reaches_the_generator() {
        let generator = CountingGenerator::new();
        let (decided, why) = generator.translator().decide_query("   ").await;
        assert_eq!(generator.calls(), 0);
        assert_eq!(decided, "   ");
        assert!(why.is_none());
    }

    /// No model installed: the search still runs on the author's own words, and
    /// the reason is rendered for `diagnostics` rather than swallowed (02 §6).
    #[tokio::test]
    async fn a_missing_model_degrades_with_a_reason_for_diagnostics() {
        let translator = Translator {
            generators: None,
            model_id: "none".to_string(),
            policy: DataPolicy::LocalOnly,
        };
        let query = "где вызывается register_embedder_representation";
        let (decided, why) = translator.decide_query(query).await;
        assert_eq!(decided, query, "the query is searched as written");
        let label = why.expect("a reason is produced").label();
        assert!(label.starts_with("no_generator"), "{label}");
    }
}
