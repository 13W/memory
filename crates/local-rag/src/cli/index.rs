//! `local-rag index <path>` / `local-rag reindex` (spec 11 §6, spec 06 §1) —
//! scan a worktree into a searchable generation: reconcile (scan + build) →
//! embed (backfill) → activate (switch) → materialize the FTS view.
//!
//! [`index_worktree`]/[`project_generation`] are real product code built from
//! the same primitives `crates/xtask/src/bench/run.rs`'s dev-only benchmark
//! harness already composes — this module is that pipeline's first
//! production caller (T15-07). [`IndexCtx`] is deliberately reusable beyond
//! this file: `cli::watch` (also T15-07) drives the same two functions on
//! every file-change trigger instead of duplicating the pipeline.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use local_rag::daemon::gitroot;
use local_rag_core::config::Config;
use local_rag_core::identity::{SystemUuidV7, Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_embed::{
    BackfillError, BackfillParams, BackfillReport, Embedder, InFlight, ProviderEntry, ProviderPool,
};
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::{
    MetaError, ReconcileError, ReconcileReport, WorktreeMeta, load_worktree_meta, reconcile_once,
};
use local_rag_index::scan::{ScanMode, StatCache};
use local_rag_models::{DEFAULT_MODEL_ID, OnnxEmbedder, find, is_installed};
use local_rag_projection::{
    BruteForceProjectionStore, CacheVectorSource, ModelSwitchError, SwitchError, SwitchOutcome,
    params_for_model_space, shard_dir, switch,
};
use local_rag_store::{
    CacheDb, Candidate, DEFAULT_MODEL_SPACE_ID, FtsMaterializeError, FtsMaterializeOutcome,
    RequestRoot, Resolution, RetentionParams, StateDb, WorktreeRootFacts, WriteError,
    create_repository, create_worktree, effective_data_policy, ensure_store_instance_uuid,
    insert_projection_state, materialize_fts, observe_repository_path, observe_worktree_path,
    resolve, worktree_summary,
};

use super::{block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

/// Everything the pipeline needs, built once per invocation and threaded
/// through both [`index_worktree`] and [`project_generation`].
pub(crate) struct IndexCtx {
    pub state: Arc<StateDb>,
    pub cache: Arc<CacheDb>,
    pub layout: StoreLayout,
    pub uuids: Arc<dyn UuidSource + Send + Sync>,
    pub embedder: Arc<dyn Embedder>,
    pub model_space_id: Uuid,
    pub retention: RetentionParams,
    pub data_policy: local_rag_core::config::DataPolicy,
    pub classifier: ClassifierConfig,
}

/// Why a step of the index/reindex pipeline failed.
#[derive(Debug)]
pub(crate) enum IndexError {
    Meta(MetaError),
    WorktreeVanished,
    InvalidGenerationId,
    Reconcile(ReconcileError),
    ModelSpaceParams(ModelSwitchError),
    Backfill(BackfillError),
    VectorSource(rusqlite::Error),
    Switch(SwitchError),
    Fts(FtsMaterializeError),
    EffectivePolicy(rusqlite::Error),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Meta(e) => write!(f, "could not load worktree metadata: {e}"),
            IndexError::WorktreeVanished => {
                write!(f, "the worktree vanished from the registry mid-run")
            }
            IndexError::InvalidGenerationId => {
                write!(f, "internal error: generation id is not a UUID")
            }
            IndexError::Reconcile(e) => write!(f, "scan/build failed: {e}"),
            IndexError::ModelSpaceParams(ModelSwitchError::NoShardParams { model_space_id }) => {
                write!(
                    f,
                    "model space {model_space_id} has no code_raw representation \
                     registered yet; run `local-rag init --download-models` first"
                )
            }
            IndexError::ModelSpaceParams(e) => write!(f, "{e}"),
            IndexError::Backfill(e) => write!(f, "embedding backfill failed: {e}"),
            IndexError::VectorSource(e) => write!(f, "could not read cached vectors: {e}"),
            IndexError::Switch(e) => write!(f, "activating the new generation failed: {e}"),
            IndexError::Fts(e) => write!(f, "FTS materialization failed: {e}"),
            IndexError::EffectivePolicy(e) => {
                write!(f, "could not resolve the effective data_policy: {e}")
            }
        }
    }
}

/// The effect of projecting one already-built generation onto both search
/// legs (dense shard + FTS view).
#[derive(Debug)]
pub(crate) struct ProjectOutcome {
    pub backfill: BackfillReport,
    pub switch: SwitchOutcome,
    pub fts: FtsMaterializeOutcome,
}

/// The effective `data_policy` (spec 02 §3.2, T16-01) for indexing
/// `worktree_id`: `global` folded with its owning repository's own stricter
/// setting, if any (`local_rag_store::effective_data_policy`). A repository
/// can only tighten, never relax, the global default. A worktree row that no
/// longer exists is `IndexError::WorktreeVanished` — the same condition this
/// module already names for a worktree disappearing mid-run elsewhere.
fn effective_data_policy_for_worktree(
    conn: &rusqlite::Connection,
    global: local_rag_core::config::DataPolicy,
    worktree_id: &str,
) -> Result<local_rag_core::config::DataPolicy, IndexError> {
    let summary = worktree_summary(conn, worktree_id)
        .map_err(IndexError::EffectivePolicy)?
        .ok_or(IndexError::WorktreeVanished)?;
    effective_data_policy(global, conn, &[summary.repo_id.as_str()])
        .map_err(IndexError::EffectivePolicy)
}

/// Embed → activate → materialize a generation that [`reconcile_once`] has
/// already built (state `projection_ready`).
///
/// `params_for_model_space` is checked first, and deliberately fails fast
/// here rather than after a — potentially large — embedding run: a model
/// space with no registered `code_raw` representation cannot open a shard no
/// matter how the backfill goes, so there is nothing to gain from running it
/// first.
pub(crate) async fn project_generation(
    ctx: &IndexCtx,
    worktree_id: Uuid,
    generation_id: Uuid,
    now_ms: i64,
) -> Result<ProjectOutcome, IndexError> {
    let (params, policy) = {
        let read = ctx
            .state
            .open_read()
            .map_err(|e| IndexError::ModelSpaceParams(ModelSwitchError::Open(e)))?;
        let params = params_for_model_space(&read, &ctx.model_space_id)
            .map_err(IndexError::ModelSpaceParams)?;
        let policy =
            effective_data_policy_for_worktree(&read, ctx.data_policy, &worktree_id.to_string())?;
        (params, policy)
    };

    let pool = ProviderPool::new(vec![ProviderEntry::local("cli", ctx.embedder.clone())]);
    let backfill = local_rag_embed::run_backfill(
        &ctx.state,
        &ctx.cache,
        &pool,
        policy,
        &ctx.model_space_id.to_string(),
        &BackfillParams::default(),
        &ctx.retention,
        &InFlight::new(),
        now_ms,
    )
    .await
    .map_err(IndexError::Backfill)?;

    let switch_outcome = {
        let read = ctx
            .state
            .open_read()
            .map_err(|e| IndexError::ModelSpaceParams(ModelSwitchError::Open(e)))?;
        let vectors = CacheVectorSource::new(
            &ctx.state,
            &ctx.cache,
            &read,
            &generation_id,
            &ctx.model_space_id,
        )
        .map_err(IndexError::VectorSource)?;
        switch(
            &ctx.state,
            &BruteForceProjectionStore::new(),
            &shard_dir(&ctx.layout, &worktree_id, &ctx.model_space_id),
            params,
            worktree_id,
            generation_id,
            ctx.model_space_id,
            &vectors,
            ctx.uuids.as_ref(),
            now_ms,
        )
        .await
        .map_err(IndexError::Switch)?
    };

    let fts = materialize_fts(
        &ctx.state,
        &ctx.cache,
        &worktree_id.to_string(),
        &generation_id.to_string(),
        now_ms,
    )
    .await
    .map_err(IndexError::Fts)?;

    Ok(ProjectOutcome {
        backfill,
        switch: switch_outcome,
        fts,
    })
}

/// What one `index`/`reindex` run produced.
#[derive(Debug)]
pub(crate) struct IndexOutcome {
    pub reconcile: ReconcileReport,
    pub project: ProjectOutcome,
}

/// Scan `meta` into a new generation and project it (spec 06 §1 → spec 05).
pub(crate) async fn index_worktree(
    ctx: &IndexCtx,
    meta: &WorktreeMeta,
    stat_cache: &mut StatCache,
    classifier: &ClassifierConfig,
    scanner: &Scanner,
    now_ms: i64,
) -> Result<IndexOutcome, IndexError> {
    let worktree_id: Uuid = meta
        .worktree_id
        .parse()
        .map_err(|_| IndexError::WorktreeVanished)?;

    let report = reconcile_once(
        &ctx.state,
        meta,
        ScanMode::Strict,
        stat_cache,
        classifier,
        scanner,
        ctx.uuids.as_ref(),
        now_ms,
    )
    .await
    .map_err(IndexError::Reconcile)?;

    let generation_id: Uuid = report
        .build
        .generation_id
        .parse()
        .map_err(|_| IndexError::InvalidGenerationId)?;

    let project = project_generation(ctx, worktree_id, generation_id, now_ms).await?;

    Ok(IndexOutcome {
        reconcile: report,
        project,
    })
}

/// Open `state.sqlite` — the one piece of [`IndexCtx`] needed before worktree
/// identity is even known, split out so resolution (and the `Ambiguous`/
/// `GlobalOnly`-refusal exits) never pays for opening the embedder first.
pub(crate) fn open_state(layout: &StoreLayout) -> Result<Arc<StateDb>, String> {
    // The daemon's own startup does `ensure → open` (spec 02 §4.1); a
    // one-shot CLI command against a store `serve` has never touched needs
    // the identical ordering, or `StateDb::open` fails outright (SQLite
    // cannot create a file inside a directory that does not exist yet).
    layout
        .ensure()
        .map_err(|e| format!("could not prepare the store directory: {e}"))?;
    StateDb::open(layout.state_db())
        .map(Arc::new)
        .map_err(|e| format!("could not open state.sqlite: {e}"))
}

/// Open `cache.sqlite` against `state`'s own canonical store instance id
/// (`ensure_store_instance_uuid`, minted once and reused thereafter) — the
/// same two-step `daemon::lifecycle::run` itself does at startup. Shared by
/// every CLI command that reads `embedding_cache`, whether or not it also
/// needs a live embedder (`cli::rebuild --dense` reads only already-cached
/// vectors, never opening `OnnxEmbedder` at all).
pub(crate) async fn open_cache(
    state: &StateDb,
    layout: &StoreLayout,
) -> Result<Arc<CacheDb>, String> {
    let candidate = SystemUuidV7.next_uuid().to_string();
    let store_instance_uuid = state
        .writer()
        .transaction(move |tx| ensure_store_instance_uuid(tx, &candidate))
        .await
        .map_err(|e| format!("could not read the store instance id: {e}"))?;
    CacheDb::open(layout.cache_db(), &store_instance_uuid)
        .map(Arc::new)
        .map_err(|e| format!("could not open cache.sqlite: {e}"))
}

/// Resolve `facts` against the registry (spec 02 §3.3), with no `repo_hint`
/// — `index`/`reindex` never carry one; that is specifically `repo attach`'s
/// job (spec 11 §6).
pub(crate) fn resolve_facts(
    state: &StateDb,
    facts: &WorktreeRootFacts,
) -> Result<Resolution, String> {
    let conn = state
        .open_read()
        .map_err(|e| format!("could not open state.sqlite: {e}"))?;
    resolve(
        &conn,
        &RequestRoot {
            worktree_root: Some(facts.clone()),
            repo_hint: None,
        },
    )
    .map_err(|e| format!("could not resolve worktree identity: {e}"))
}

/// Complete an already-open `state` into a full [`IndexCtx`]: open the
/// installed default model, `cache.sqlite`, and the retention/data-policy
/// knobs an operator has already set — the same disk-state gate `cli::init`
/// uses (an uninstalled model is a clear error here, not a silent
/// lexical-only fallback: `index`/`reindex` write real, permanent state, so a
/// half-configured store must refuse rather than write vectors under a
/// representation nothing will ever query with).
pub(crate) async fn finish_index_ctx(
    state: Arc<StateDb>,
    layout: &StoreLayout,
    config: &Config,
) -> Result<IndexCtx, String> {
    let entry = find(DEFAULT_MODEL_ID)
        .ok_or_else(|| format!("model {DEFAULT_MODEL_ID:?} is not in this build's catalog"))?;
    if !is_installed(layout, entry.model_id) {
        return Err(format!(
            "{} is not installed; run `local-rag init --download-models` first",
            entry.model_id
        ));
    }
    let embedder: Arc<dyn Embedder> = Arc::new(OnnxEmbedder::open(layout, entry).map_err(|e| {
        format!(
            "{} is installed but could not be opened: {e}",
            entry.model_id
        )
    })?);

    let cache = open_cache(&state, layout).await?;

    let model_space_id: Uuid = DEFAULT_MODEL_SPACE_ID
        .parse()
        .expect("DEFAULT_MODEL_SPACE_ID is a valid UUID");

    Ok(IndexCtx {
        state,
        cache,
        layout: layout.clone(),
        uuids: Arc::new(SystemUuidV7),
        embedder,
        model_space_id,
        retention: RetentionParams::from_storage_config(&config.storage),
        data_policy: config.models.data_policy,
        classifier: ClassifierConfig::from_index_config(&config.index),
    })
}

/// Register a brand-new `{repo_id, worktree_id}` under `facts` — the same
/// four-write transaction `xtask::bench::run::register_worktree` already
/// established (create the repository and worktree rows, observe both
/// current paths, seed `worktree_projection_state`), now real product code
/// rather than a dev-only benchmark helper.
pub(crate) async fn register_new_worktree(
    state: &StateDb,
    repo_id: Uuid,
    worktree_id: Uuid,
    facts: &WorktreeRootFacts,
    now_ms: i64,
) -> Result<(), WriteError> {
    let (r, w) = (repo_id.to_string(), worktree_id.to_string());
    let (canonical, display, fp, kind) = (
        facts.observed_canonical_path.clone(),
        facts.display_path.clone(),
        facts.path_fingerprint.clone(),
        facts.kind,
    );
    state
        .writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, now_ms)?;
            create_worktree(tx, &w, &r, kind, now_ms)?;
            observe_worktree_path(tx, &w, &canonical, &display, &fp, now_ms)?;
            observe_repository_path(tx, &r, &canonical, now_ms)?;
            insert_projection_state(tx, &w, now_ms)
        })
        .await
}

pub(crate) fn print_ambiguous(candidates: &[Candidate]) {
    eprintln!(
        "{BIN}: this path matches {} detached worktree(s) of more than one repository; \
         pick one with `local-rag repo attach <repo_id> --worktree <worktree_id>`:",
        candidates.len()
    );
    for c in candidates {
        eprintln!(
            "  repo {} worktree {} ({})",
            c.repo_id,
            c.worktree_id,
            c.kind.as_str()
        );
    }
}

/// Run the full pipeline for an already-resolved `worktree_id`, printing a
/// one-line summary on success.
async fn run_pipeline(ctx: &IndexCtx, worktree_id: Uuid, now_ms: i64) -> ExitCode {
    let case = gitroot::case_sensitivity();
    let meta = match load_worktree_meta(&ctx.state, &worktree_id.to_string(), case) {
        Ok(Some(meta)) => meta,
        Ok(None) => return fail(BIN, &IndexError::WorktreeVanished.to_string()),
        Err(e) => return fail(BIN, &IndexError::Meta(e).to_string()),
    };

    let mut stat_cache = StatCache::new();
    let scanner = Scanner::new();

    match index_worktree(
        ctx,
        &meta,
        &mut stat_cache,
        &ctx.classifier,
        &scanner,
        now_ms,
    )
    .await
    {
        Ok(outcome) => {
            println!(
                "{BIN}: indexed {} files ({} occurrences) into generation {}; \
                 embedded {} subjects ({} reused, {} repaired, {} failed); \
                 dense +{}/-{}; fts {} occurrences",
                outcome.reconcile.build.files_indexed,
                outcome.reconcile.build.occurrences,
                outcome.reconcile.build.generation_id,
                outcome.project.backfill.embedded,
                outcome.project.backfill.reused,
                outcome.project.backfill.repaired,
                outcome.project.backfill.failed,
                outcome.project.switch.upserted,
                outcome.project.switch.deleted,
                outcome.project.fts.occurrence_count,
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(BIN, &e.to_string()),
    }
}

#[derive(Debug, clap::Args)]
pub struct IndexArgs {
    /// Directory to index (registered as a new worktree if not already known).
    path: String,
}

pub fn run_index(args: IndexArgs) -> ExitCode {
    let (layout, config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    let path = PathBuf::from(&args.path);
    let Some(facts) = gitroot::probe(&path) else {
        return fail(BIN, &format!("{}: not an accessible directory", args.path));
    };

    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let resolution = match resolve_facts(&state, &facts) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &e),
    };
    let now_ms = system_now_ms();

    block_on(async {
        let worktree_id = match resolution {
            Resolution::Resolved { worktree_id, .. } => match worktree_id.parse::<Uuid>() {
                Ok(id) => id,
                Err(_) => return fail(BIN, "internal error: stored worktree id is not a UUID"),
            },
            Resolution::GlobalOnly => {
                let repo_id = SystemUuidV7.next_uuid();
                let worktree_id = SystemUuidV7.next_uuid();
                if let Err(e) =
                    register_new_worktree(&state, repo_id, worktree_id, &facts, now_ms).await
                {
                    return fail(BIN, &format!("could not register the worktree: {e}"));
                }
                worktree_id
            }
            Resolution::Ambiguous { candidates } => {
                print_ambiguous(&candidates);
                return ExitCode::FAILURE;
            }
        };

        let ctx = match finish_index_ctx(state, &layout, &config).await {
            Ok(ctx) => ctx,
            Err(e) => return fail(BIN, &e),
        };
        run_pipeline(&ctx, worktree_id, now_ms).await
    })
}

pub fn run_reindex() -> ExitCode {
    let (layout, config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            return fail(
                BIN,
                &format!("could not determine the current directory: {e}"),
            );
        }
    };
    let Some(facts) = gitroot::probe(&cwd) else {
        return fail(BIN, "the current directory is not accessible");
    };

    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };
    let resolution = match resolve_facts(&state, &facts) {
        Ok(r) => r,
        Err(e) => return fail(BIN, &e),
    };
    let now_ms = system_now_ms();

    let worktree_id = match resolution {
        Resolution::Resolved { worktree_id, .. } => match worktree_id.parse::<Uuid>() {
            Ok(id) => id,
            Err(_) => return fail(BIN, "internal error: stored worktree id is not a UUID"),
        },
        Resolution::GlobalOnly => {
            return fail(
                BIN,
                "this path is not indexed yet; run `local-rag index <path>` first",
            );
        }
        Resolution::Ambiguous { candidates } => {
            print_ambiguous(&candidates);
            return ExitCode::FAILURE;
        }
    };

    block_on(async {
        let ctx = match finish_index_ctx(state, &layout, &config).await {
            Ok(ctx) => ctx,
            Err(e) => return fail(BIN, &e),
        };
        run_pipeline(&ctx, worktree_id, now_ms).await
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use local_rag_core::identity::domain::path_fingerprint;
    use local_rag_core::identity::path::CaseSensitivity;
    use local_rag_core::identity::uuidv7_from;
    use local_rag_embed::HashingEmbedder;
    use local_rag_protocol::SearchMode;
    use local_rag_search::{QueryEmbedder, SearchRequest};
    use local_rag_store::{
        RepresentationKind, WorktreeKind, register_representation, set_model_space_representation,
        set_repo_data_policy,
    };
    use local_rag_test_support::TempHome;

    use super::*;

    /// A deterministic, non-random UUID source — the same "monotone UUIDv7"
    /// convention `xtask::bench::run::SeqUuids` and
    /// `crates/projection/tests/rebuild.rs::SeqUuidV7` already establish.
    struct SeqUuids {
        counter: AtomicU64,
    }

    impl SeqUuids {
        fn new() -> Self {
            SeqUuids {
                counter: AtomicU64::new(0),
            }
        }
    }

    impl UuidSource for SeqUuids {
        fn next_uuid(&self) -> Uuid {
            let n = self.counter.fetch_add(1, Ordering::Relaxed);
            uuidv7_from(9_000_000 + n, [0x37; 10])
        }
    }

    /// A fixture [`IndexCtx`] backed by [`HashingEmbedder`] (real, deterministic,
    /// no ONNX/network) — the same fixture-provider convention `local_rag_embed`'s
    /// own module doc names for exactly this purpose ("provider-pool behavior is
    /// testable without a 100 MB asset").
    fn open_ctx(layout: &StoreLayout) -> IndexCtx {
        let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
        let cache =
            Arc::new(CacheDb::open(layout.cache_db(), "test-instance").expect("open cache.sqlite"));
        IndexCtx {
            state,
            cache,
            layout: layout.clone(),
            uuids: Arc::new(SeqUuids::new()),
            embedder: Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw)),
            model_space_id: DEFAULT_MODEL_SPACE_ID.parse().expect("valid UUID"),
            retention: RetentionParams {
                keep_last_k: 2,
                window_ms: 7 * 24 * 60 * 60 * 1000,
            },
            data_policy: local_rag_core::config::DataPolicy::LocalOnly,
            classifier: ClassifierConfig::new(1024 * 1024),
        }
    }

    async fn register_code_raw(ctx: &IndexCtx, now_ms: i64) {
        let key = ctx.embedder.key();
        ctx.state
            .writer()
            .transaction(move |tx| {
                let id = register_representation(tx, "test-code-raw", &key, now_ms)?;
                set_model_space_representation(
                    tx,
                    DEFAULT_MODEL_SPACE_ID,
                    RepresentationKind::CodeRaw,
                    &id,
                    true,
                    now_ms,
                )
            })
            .await
            .expect("register representation");
    }

    fn facts_for(root: &std::path::Path) -> WorktreeRootFacts {
        let path = root.display().to_string();
        WorktreeRootFacts {
            observed_canonical_path: path.clone(),
            display_path: path.clone(),
            path_fingerprint: path_fingerprint(&path),
            kind: WorktreeKind::NonGit,
            common_dir_fingerprint: None,
            remote_fingerprint: None,
        }
    }

    #[tokio::test]
    async fn indexing_a_fresh_worktree_produces_a_searchable_generation() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        std::fs::write(
            root.join("main.rs"),
            "fn parse_config(path: &Path) -> Config { unimplemented!() }",
        )
        .expect("seed file");

        let ctx = open_ctx(&layout);
        let now_ms = 1_000;
        register_code_raw(&ctx, now_ms).await;

        let repo_id = ctx.uuids.next_uuid();
        let worktree_id = ctx.uuids.next_uuid();
        let facts = facts_for(&root);
        register_new_worktree(&ctx.state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register worktree");

        let meta = load_worktree_meta(
            &ctx.state,
            &worktree_id.to_string(),
            CaseSensitivity::Sensitive,
        )
        .expect("load meta")
        .expect("worktree exists");
        let mut stat_cache = StatCache::new();
        let scanner = Scanner::new();
        let outcome = index_worktree(
            &ctx,
            &meta,
            &mut stat_cache,
            &ctx.classifier,
            &scanner,
            now_ms,
        )
        .await
        .expect("index");

        assert_eq!(outcome.reconcile.build.files_indexed, 1);
        assert!(
            outcome.project.backfill.embedded >= 1,
            "nothing embedded: {:?}",
            outcome.project.backfill
        );
        assert!(outcome.project.switch.upserted >= 1);
        assert_eq!(
            outcome.project.fts.occurrence_count,
            outcome.reconcile.build.occurrences as u64
        );

        // Prove it end to end through the real production `SearchEngine` —
        // the same `build_search_engine` `main.rs::serve` itself uses.
        let query_embedder: Arc<dyn QueryEmbedder> =
            Arc::new(local_rag::daemon::EmbedderQueryAdapter::new(
                HashingEmbedder::new(RepresentationKind::CodeRaw),
            ));
        let engine = local_rag::daemon::search::build_search_engine(
            ctx.state.clone(),
            ctx.cache.clone(),
            ctx.layout.clone(),
            ctx.uuids.clone(),
            query_embedder,
            8,
        );
        let response = engine
            .search_code(
                SearchRequest {
                    root: RequestRoot {
                        worktree_root: Some(facts),
                        repo_hint: None,
                    },
                    query: "parse_config".to_string(),
                    mode: SearchMode::Hybrid,
                    limit: 5,
                    name_pattern: None,
                },
                now_ms,
            )
            .await
            .expect("no infra error")
            .expect("no domain error");
        assert!(
            response.results.iter().any(|r| r.path.ends_with("main.rs")),
            "{:?}",
            response.results
        );
    }

    #[tokio::test]
    async fn reindexing_after_a_file_change_builds_a_new_generation() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        let file = root.join("lib.rs");
        std::fs::write(&file, "fn one() {}").expect("seed file");

        let ctx = open_ctx(&layout);
        let now_ms = 1_000;
        register_code_raw(&ctx, now_ms).await;
        let repo_id = ctx.uuids.next_uuid();
        let worktree_id = ctx.uuids.next_uuid();
        let facts = facts_for(&root);
        register_new_worktree(&ctx.state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register worktree");

        let case = CaseSensitivity::Sensitive;
        let mut stat_cache = StatCache::new();
        let scanner = Scanner::new();

        let meta = load_worktree_meta(&ctx.state, &worktree_id.to_string(), case)
            .expect("load meta")
            .expect("worktree exists");
        let first = index_worktree(
            &ctx,
            &meta,
            &mut stat_cache,
            &ctx.classifier,
            &scanner,
            now_ms,
        )
        .await
        .expect("first index");

        std::fs::write(&file, "fn one() {}\nfn two() {}").expect("modify file");
        let meta2 = load_worktree_meta(&ctx.state, &worktree_id.to_string(), case)
            .expect("load meta")
            .expect("worktree exists");
        let second = index_worktree(
            &ctx,
            &meta2,
            &mut stat_cache,
            &ctx.classifier,
            &scanner,
            now_ms + 1_000,
        )
        .await
        .expect("second index");

        assert_ne!(
            first.reconcile.build.generation_id,
            second.reconcile.build.generation_id
        );
        assert!(second.reconcile.build.generation_number > first.reconcile.build.generation_number);
    }

    #[tokio::test]
    async fn project_generation_fails_fast_without_a_registered_representation() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        std::fs::write(root.join("a.rs"), "fn a() {}").expect("seed file");

        let ctx = open_ctx(&layout);
        // Deliberately never registers a `code_raw` representation.
        let now_ms = 1_000;
        let repo_id = ctx.uuids.next_uuid();
        let worktree_id = ctx.uuids.next_uuid();
        let facts = facts_for(&root);
        register_new_worktree(&ctx.state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register worktree");
        let meta = load_worktree_meta(
            &ctx.state,
            &worktree_id.to_string(),
            CaseSensitivity::Sensitive,
        )
        .expect("load meta")
        .expect("worktree exists");
        let mut stat_cache = StatCache::new();
        let scanner = Scanner::new();

        let err = index_worktree(
            &ctx,
            &meta,
            &mut stat_cache,
            &ctx.classifier,
            &scanner,
            now_ms,
        )
        .await
        .expect_err("no representation is registered");
        assert!(
            matches!(
                err,
                IndexError::ModelSpaceParams(ModelSwitchError::NoShardParams { .. })
            ),
            "{err}"
        );
        assert!(err.to_string().contains("init --download-models"), "{err}");
    }

    /// T16-01: `project_generation` must resolve the *effective* `data_policy`
    /// for the worktree being indexed — global folded with its owning
    /// repository's own stricter setting — not just pass `ctx.data_policy`
    /// straight through. A repository can only tighten, never relax it.
    #[tokio::test]
    async fn effective_policy_folds_in_the_worktree_owning_repositorys_stricter_setting() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");

        let mut ctx = open_ctx(&layout);
        ctx.data_policy = local_rag_core::config::DataPolicy::AllowRemoteFull;
        let now_ms = 1_000;

        let repo_id = ctx.uuids.next_uuid();
        let worktree_id = ctx.uuids.next_uuid();
        let facts = facts_for(&root);
        register_new_worktree(&ctx.state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register worktree");

        // Before any repository-level override, the effective policy is just
        // the global one.
        {
            let read = ctx.state.open_read().expect("read conn");
            let policy = effective_data_policy_for_worktree(
                &read,
                ctx.data_policy,
                &worktree_id.to_string(),
            )
            .expect("resolve effective policy");
            assert_eq!(policy, local_rag_core::config::DataPolicy::AllowRemoteFull);
        }

        // The repository tightens its own policy below the lax global
        // default.
        let repo_id_string = repo_id.to_string();
        ctx.state
            .writer()
            .transaction(move |tx| {
                set_repo_data_policy(
                    tx,
                    &repo_id_string,
                    local_rag_core::config::DataPolicy::LocalOnly,
                )
            })
            .await
            .expect("tighten repository policy");

        let read = ctx.state.open_read().expect("read conn");
        let policy =
            effective_data_policy_for_worktree(&read, ctx.data_policy, &worktree_id.to_string())
                .expect("resolve effective policy");
        assert_eq!(
            policy,
            local_rag_core::config::DataPolicy::LocalOnly,
            "the repository's stricter setting must win over the lax global default"
        );
    }

    /// A worktree row that no longer exists is the same `WorktreeVanished`
    /// this module already names for a worktree disappearing mid-run
    /// elsewhere, not a fresh error shape.
    #[tokio::test]
    async fn effective_policy_of_an_unknown_worktree_is_worktree_vanished() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let ctx = open_ctx(&layout);

        let read = ctx.state.open_read().expect("read conn");
        let bogus_worktree_id = ctx.uuids.next_uuid().to_string();
        let err = effective_data_policy_for_worktree(&read, ctx.data_policy, &bogus_worktree_id)
            .expect_err("no such worktree");
        assert!(matches!(err, IndexError::WorktreeVanished), "{err}");
    }
}
