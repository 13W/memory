//! The indexing pipeline (spec 06 §1–2, spec 05 §5) — scan a worktree into a
//! searchable generation: reconcile (scan + build) → embed (backfill) →
//! activate (switch) → materialize the FTS view.
//!
//! T20-02 moved this out of the binary's own `cli/index.rs`, where it was
//! `pub(crate)` to a target no library code can link against. Nothing about
//! its semantics changed with the move; what changed is that the pipeline now
//! has **two callers**, not one: the CLI (`cli::index`'s `index`/`reindex`,
//! `cli::watch`'s per-trigger projection, `cli::rebuild`'s
//! [`open_state`]/[`open_cache`]) and the daemon's future per-worktree
//! background tasks (`daemon::indexing`, T20-05/T20-06 — not implemented
//! yet). It is still the same pipeline `crates/xtask/src/bench/run.rs`'s
//! dev-only benchmark harness composes from the same primitives; [`IndexCtx`]
//! is still the one context [`index_worktree`] and [`project_generation`]
//! both thread through.
//!
//! The [`open_state`] → [`resolve_facts`] → [`finish_index_ctx`] split is
//! load-bearing for every caller, not an accident of the CLI's argument
//! order: worktree identity is resolved — and an `Ambiguous`/`GlobalOnly`
//! path refused — *before* the embedder is opened, so no caller pays for an
//! ONNX session it is about to throw away.
//!
//! This module deliberately depends on nothing in [`crate::daemon`]: the
//! dependency runs daemon → indexing and never the other way. That is also
//! why [`index_worktree`] takes an already-loaded `WorktreeMeta` rather than
//! loading one itself — `CaseSensitivity` is `daemon::gitroot`'s own
//! out-of-band probe, and it stays with whoever is driving the pipeline.
//! Nothing here prints: rendering an outcome (a summary line, an exit code, a
//! tracing event) belongs to the caller.

use std::future::Future;
use std::sync::Arc;

use local_rag_core::config::Config;
use local_rag_core::identity::{SystemUuidV7, Uuid, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_embed::{
    BackfillError, BackfillParams, BackfillReport, Embedder, InFlight, ProviderEntry, ProviderPool,
};
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::{
    MetaError, ReconcileError, ReconcileReport, WorktreeMeta, reconcile_once,
};
use local_rag_index::scan::{ScanMode, StatCache};
use local_rag_models::{DEFAULT_MODEL_ID, OnnxEmbedder, find, is_installed};
use local_rag_projection::{
    BruteForceProjectionStore, CacheVectorSource, ModelSwitchError, SwitchError, SwitchOutcome,
    params_for_model_space, shard_dir, switch,
};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, FtsMaterializeError, FtsMaterializeOutcome, RequestRoot,
    Resolution, RetentionParams, StateDb, WorktreeLockRegistry, WorktreeRootFacts, WriteError,
    create_repository, create_worktree, effective_data_policy, ensure_store_instance_uuid,
    insert_projection_state, materialize_fts, observe_repository_path, observe_worktree_path,
    register_managed_worktree, resolve, worktree_summary,
};

/// Everything the pipeline needs, built once per invocation and threaded
/// through both [`index_worktree`] and [`project_generation`].
pub struct IndexCtx {
    pub state: Arc<StateDb>,
    pub cache: Arc<CacheDb>,
    pub layout: StoreLayout,
    pub uuids: Arc<dyn UuidSource + Send + Sync>,
    pub embedder: Arc<dyn Embedder>,
    /// The same installed model, opened again under its `memory`
    /// representation key (D-036) — a second `ProviderEntry` so
    /// `run_backfill` also covers `SubjectKind::MemoryEntry` subjects, not
    /// only code ones.
    pub memory_embedder: Arc<dyn Embedder>,
    pub model_space_id: Uuid,
    pub retention: RetentionParams,
    pub data_policy: local_rag_core::config::DataPolicy,
    pub classifier: ClassifierConfig,
}

/// Why a step of the index/reindex pipeline failed.
#[derive(Debug)]
pub enum IndexError {
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
pub struct ProjectOutcome {
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
pub async fn project_generation(
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

    let pool = ProviderPool::new(vec![
        ProviderEntry::local("cli", ctx.embedder.clone()),
        ProviderEntry::local("cli", ctx.memory_embedder.clone()),
    ]);
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
pub struct IndexOutcome {
    pub reconcile: ReconcileReport,
    pub project: ProjectOutcome,
}

/// Scan `meta` into a new generation and project it (spec 06 §1 → spec 05).
pub async fn index_worktree(
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

/// Run `body` — the full write cycle for one worktree (`reconcile_once` →
/// [`project_generation`], or [`index_worktree`], which is exactly that pair)
/// — inside `locks`'s `L2.write` guard (spec 02 §5: "`L2.write` → compute →
/// L4a tx"; "only one writer per worktree ever exists `[FIXED]`").
///
/// T20-04's typed entry point for the daemon's per-worktree indexing task
/// (T20-05): callers should never reach for
/// `local_rag_store::lock::WorktreeLockRegistry::write` directly for this
/// cycle, so the "the whole reconcile→project cycle is one write-locked unit"
/// policy lives in one place instead of being each future caller's own
/// discipline to remember.
pub async fn write_locked<Fut>(
    locks: &WorktreeLockRegistry,
    worktree_id: &str,
    body: Fut,
) -> Fut::Output
where
    Fut: Future,
{
    locks.write(worktree_id, body).await
}

/// Open `state.sqlite` — the one piece of [`IndexCtx`] needed before worktree
/// identity is even known, split out so resolution (and the `Ambiguous`/
/// `GlobalOnly`-refusal exits) never pays for opening the embedder first.
pub fn open_state(layout: &StoreLayout) -> Result<Arc<StateDb>, String> {
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
pub async fn open_cache(state: &StateDb, layout: &StoreLayout) -> Result<Arc<CacheDb>, String> {
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
pub fn resolve_facts(state: &StateDb, facts: &WorktreeRootFacts) -> Result<Resolution, String> {
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
///
/// Opens its own two ONNX sessions — correct for this function's only caller
/// today, a one-shot CLI process. A future daemon-side caller (T20-05) MUST
/// NOT repeat that here: it should pass the sessions already open on
/// `daemon::embedder_provider::LazyEmbedderProvider` (T20-03) instead,
/// keeping the daemon's own two-session ceiling.
pub async fn finish_index_ctx(
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
    let memory_embedder: Arc<dyn Embedder> =
        Arc::new(OnnxEmbedder::open_for_memory(layout, entry).map_err(|e| {
            format!(
                "{} is installed but could not be opened for its memory representation: {e}",
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
        memory_embedder,
        model_space_id,
        retention: RetentionParams::from_storage_config(&config.storage),
        data_policy: config.models.data_policy,
        classifier: ClassifierConfig::from_index_config(&config.index),
    })
}

/// The write body `register_new_worktree`/`register_new_managed_worktree`
/// share — create the repository and worktree rows, observe both current
/// paths, seed `worktree_projection_state`. Factored out so both public
/// entry points run it inside exactly one `StateWriter::transaction`, never
/// two: `register_new_managed_worktree` (T20-08) additionally enrolls the
/// new worktree as managed in that same transaction, which a second,
/// separate transaction could not guarantee (a crash between the two would
/// leave a worktree registered but never marked managed).
fn new_worktree_tx(
    tx: &rusqlite::Transaction<'_>,
    repo_id: &str,
    worktree_id: &str,
    facts: &WorktreeRootFacts,
    now_ms: i64,
) -> rusqlite::Result<()> {
    create_repository(tx, repo_id, None, now_ms)?;
    create_worktree(tx, worktree_id, repo_id, facts.kind, now_ms)?;
    observe_worktree_path(
        tx,
        worktree_id,
        &facts.observed_canonical_path,
        &facts.display_path,
        &facts.path_fingerprint,
        now_ms,
    )?;
    observe_repository_path(tx, repo_id, &facts.observed_canonical_path, now_ms)?;
    insert_projection_state(tx, worktree_id, now_ms)
}

/// Register a brand-new `{repo_id, worktree_id}` under `facts` — the same
/// four-write transaction `xtask::bench::run::register_worktree` already
/// established (create the repository and worktree rows, observe both
/// current paths, seed `worktree_projection_state`), now real product code
/// rather than a dev-only benchmark helper.
pub async fn register_new_worktree(
    state: &StateDb,
    repo_id: Uuid,
    worktree_id: Uuid,
    facts: &WorktreeRootFacts,
    now_ms: i64,
) -> Result<(), WriteError> {
    let (r, w) = (repo_id.to_string(), worktree_id.to_string());
    let facts = facts.clone();
    state
        .writer()
        .transaction(move |tx| new_worktree_tx(tx, &r, &w, &facts, now_ms))
        .await
}

/// [`register_new_worktree`], plus enrolling the new worktree as daemon-managed
/// (`local_rag_store::register_managed_worktree`) in the **same** transaction
/// (T20-08, `local-rag project add` on a not-yet-known path — spec 11 §8's
/// "add … creates repo/worktree and marks it managed in one transaction").
pub async fn register_new_managed_worktree(
    state: &StateDb,
    repo_id: Uuid,
    worktree_id: Uuid,
    facts: &WorktreeRootFacts,
    now_ms: i64,
) -> Result<(), WriteError> {
    let (r, w) = (repo_id.to_string(), worktree_id.to_string());
    let facts = facts.clone();
    state
        .writer()
        .transaction(move |tx| {
            new_worktree_tx(tx, &r, &w, &facts, now_ms)?;
            register_managed_worktree(tx, &w, now_ms)
        })
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use local_rag_core::identity::domain::path_fingerprint;
    use local_rag_core::identity::path::CaseSensitivity;
    use local_rag_core::identity::uuidv7_from;
    use local_rag_embed::HashingEmbedder;
    use local_rag_index::reconcile::load_worktree_meta;
    use local_rag_protocol::{ErrorCode, SearchMode};
    use local_rag_search::{QueryEmbedder, SearchRequest};
    use local_rag_store::{
        GLOBAL_SCOPE_OWNER_ID, MemoryKind, NewMemoryEntry, RepresentationKind, ScopeKind,
        WorktreeKind, create_memory_entry, managed_worktrees, register_representation,
        set_model_space_representation, set_repo_data_policy,
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
            memory_embedder: Arc::new(HashingEmbedder::new(RepresentationKind::Memory)),
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

    async fn register_memory(ctx: &IndexCtx, now_ms: i64) {
        let key = ctx.memory_embedder.key();
        ctx.state
            .writer()
            .transaction(move |tx| {
                let id = register_representation(tx, "test-memory", &key, now_ms)?;
                set_model_space_representation(
                    tx,
                    DEFAULT_MODEL_SPACE_ID,
                    RepresentationKind::Memory,
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
        // T20-04: the full `reconcile_once → project_generation` cycle
        // (`index_worktree`) runs inside `write_locked` — the production
        // write path's own required shape (spec 02 §5). A lock-order
        // violation between `L2.write` and any `L4a` write-queue job
        // `index_worktree` triggers along the way would panic under this
        // `cargo test` debug build (`checked_scope_async`'s `debug_assert!`),
        // failing this test rather than silently passing.
        let locks = Arc::new(WorktreeLockRegistry::new());
        let outcome = write_locked(
            &locks,
            &worktree_id.to_string(),
            index_worktree(
                &ctx,
                &meta,
                &mut stat_cache,
                &ctx.classifier,
                &scanner,
                now_ms,
            ),
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
        // the same `build_search_engine` `main.rs::serve` itself uses — and
        // the same `locks` the write side just used, per T20-04's "one
        // registry, shared" contract.
        let query_embedder: Arc<dyn QueryEmbedder> =
            Arc::new(crate::daemon::EmbedderQueryAdapter::new(Arc::new(
                HashingEmbedder::new(RepresentationKind::CodeRaw),
            )));
        let engine = crate::daemon::search::build_search_engine(
            ctx.state.clone(),
            ctx.cache.clone(),
            ctx.layout.clone(),
            ctx.uuids.clone(),
            query_embedder,
            8,
            locks,
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

    /// T20-08: `register_new_managed_worktree` is `register_new_worktree`
    /// plus enrollment, atomically — both the worktree/repo rows and the
    /// `managed_worktree` row must exist after one call, in one transaction.
    #[tokio::test]
    async fn register_new_managed_worktree_creates_and_enrolls_in_one_call() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");

        let ctx = open_ctx(&layout);
        let now_ms = 1_000;
        let repo_id = ctx.uuids.next_uuid();
        let worktree_id = ctx.uuids.next_uuid();
        let facts = facts_for(&root);

        register_new_managed_worktree(&ctx.state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register managed worktree");

        let conn = ctx.state.open_read().expect("open read connection");
        let summary = worktree_summary(&conn, &worktree_id.to_string())
            .expect("read worktree")
            .expect("worktree row exists");
        assert_eq!(summary.repo_id, repo_id.to_string());

        let managed = managed_worktrees(&conn).expect("read managed_worktree");
        assert_eq!(managed.len(), 1);
        assert_eq!(managed[0].worktree_id, worktree_id.to_string());
        assert!(managed[0].enabled, "a freshly enrolled row starts enabled");
    }

    /// T20-04: `write_locked` — the typed write-cycle wrapper T20-05's
    /// per-worktree indexing task will use — inherits
    /// `WorktreeLockRegistry::write`'s per-worktree granularity: two
    /// different worktrees' write cycles run concurrently, neither blocking
    /// the other. Mirrors `crates/store/tests/lock.rs::
    /// separate_worktrees_do_not_serialize`, but drives the actual wrapper
    /// production code calls, not the bare registry.
    #[tokio::test]
    async fn write_locked_does_not_serialize_across_different_worktrees() {
        let locks = Arc::new(WorktreeLockRegistry::new());

        let (entered_a_tx, entered_a_rx) = std::sync::mpsc::channel::<()>();
        let (proceed_a_tx, proceed_a_rx) = std::sync::mpsc::channel::<()>();
        let locks_a = locks.clone();
        let task_a = tokio::spawn(async move {
            write_locked(&locks_a, "wt-1", async move {
                entered_a_tx.send(()).ok();
                tokio::task::spawn_blocking(move || proceed_a_rx.recv().ok())
                    .await
                    .ok();
            })
            .await;
        });

        let (entered_b_tx, entered_b_rx) = std::sync::mpsc::channel::<()>();
        let (proceed_b_tx, proceed_b_rx) = std::sync::mpsc::channel::<()>();
        let locks_b = locks.clone();
        let task_b = tokio::spawn(async move {
            write_locked(&locks_b, "wt-2", async move {
                entered_b_tx.send(()).ok();
                tokio::task::spawn_blocking(move || proceed_b_rx.recv().ok())
                    .await
                    .ok();
            })
            .await;
        });

        // Both must be observed entered *without releasing either* — if
        // `write_locked` regressed to one global lock, the second `recv()`
        // would never fire while the first task still holds its (different)
        // worktree's lock, and this test would hang rather than flake.
        tokio::task::spawn_blocking(move || entered_a_rx.recv().expect("A entered"))
            .await
            .expect("join A-entered wait");
        tokio::task::spawn_blocking(move || entered_b_rx.recv().expect("B entered"))
            .await
            .expect("join B-entered wait");

        proceed_a_tx.send(()).ok();
        proceed_b_tx.send(()).ok();
        task_a.await.expect("join A");
        task_b.await.expect("join B");
    }

    /// T20-04 (closes D-043): `build_search_engine`'s `SearchEngine` and
    /// `write_locked`'s writer must share the very same
    /// `Arc<WorktreeLockRegistry>`, not two independently constructed ones —
    /// the actual production wiring `daemon::lifecycle::{StartOptions,
    /// DaemonHandle}` sets up. If a regression reintroduced
    /// `build_search_engine`'s own private registry (the exact bug D-043
    /// named — `daemon/search.rs` used to construct
    /// `WorktreeLockRegistry::new()` itself), the reader below would never
    /// observe the writer holding `L2.write` at all, and this test would
    /// time out waiting for a `BUSY_RETRY` that never comes, rather than
    /// observing one promptly.
    ///
    /// No indexed content is needed: `search_code` resolves the worktree
    /// (spec 09 §1) and only then reaches `L2.read` — `resolve()` succeeds
    /// off `register_new_worktree`'s own rows alone (spec 02 §3.3), so the
    /// bounded wait times out before the pipeline would ever need a real
    /// generation. Mirrors `crates/store/tests/lock.rs::
    /// read_bounded_times_out_while_a_writer_holds_the_lock` and
    /// `crates/search/tests/pipeline.rs::
    /// writer_holding_l2_write_delays_search_past_bound_yields_busy_retry`,
    /// but through `local-rag`'s own production `build_search_engine`/
    /// `write_locked`, not a hand-assembled `SearchEngine` — with one
    /// difference forced by that production path: this crate's `Cargo.toml`
    /// deliberately curates tokio's feature set without `test-util` (see
    /// `daemon::consolidation_trigger`'s own tests for the same constraint),
    /// so `DEFAULT_L2_READ_WAIT_BUDGET` elapses for real here rather than via
    /// paused virtual time — a real, bounded (`tokio::time::timeout` inside
    /// `read_bounded` itself) wait, not a flaky guess at "long enough."
    #[tokio::test]
    async fn write_locked_blocks_the_shared_search_engines_bounded_read_until_busy_retry() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");

        let ctx = open_ctx(&layout);
        let now_ms = 1_000;
        register_code_raw(&ctx, now_ms).await;

        let repo_id = ctx.uuids.next_uuid();
        let worktree_id = ctx.uuids.next_uuid();
        let facts = facts_for(&root);
        register_new_worktree(&ctx.state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register worktree");

        let locks = Arc::new(WorktreeLockRegistry::new());
        let query_embedder: Arc<dyn QueryEmbedder> =
            Arc::new(crate::daemon::EmbedderQueryAdapter::new(Arc::new(
                HashingEmbedder::new(RepresentationKind::CodeRaw),
            )));
        let engine = crate::daemon::search::build_search_engine(
            ctx.state.clone(),
            ctx.cache.clone(),
            ctx.layout.clone(),
            ctx.uuids.clone(),
            query_embedder,
            8,
            locks.clone(),
        );

        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (proceed_tx, proceed_rx) = std::sync::mpsc::channel::<()>();
        let worktree_id_str = worktree_id.to_string();
        let writer_task = tokio::spawn(async move {
            write_locked(&locks, &worktree_id_str, async move {
                entered_tx.send(()).ok();
                tokio::task::spawn_blocking(move || proceed_rx.recv().ok())
                    .await
                    .ok();
            })
            .await;
        });
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("writer entered"))
            .await
            .expect("join entered-wait");

        let request = SearchRequest {
            root: RequestRoot {
                worktree_root: Some(facts),
                repo_hint: None,
            },
            query: "anything".to_string(),
            mode: SearchMode::Hybrid,
            limit: 5,
            name_pattern: None,
        };
        // No `test-util`/paused clock in this crate (see the doc comment
        // above): `search_code`'s own `tokio::time::timeout` genuinely waits
        // out `DEFAULT_L2_READ_WAIT_BUDGET` before this resolves — a real
        // but bounded wait, not a sleep guessing at "long enough."
        let search_task = tokio::spawn(async move { engine.search_code(request, now_ms).await });
        let outcome = search_task
            .await
            .expect("join search task")
            .expect("no infrastructure error");
        let err = outcome
            .expect_err("must be a BUSY_RETRY error envelope while the writer holds L2.write");
        assert_eq!(err.code, ErrorCode::BusyRetry);
        assert!(err.retryable);

        proceed_tx.send(()).ok();
        writer_task.await.expect("join writer task");
    }

    /// D-036: `finish_index_ctx`/`project_generation`'s backfill pool now
    /// carries a second, memory-tagged provider entry alongside the code_raw
    /// one — once `memory` is `required` for the model space, indexing a
    /// worktree also backfills whatever memory entries already exist, not
    /// only code occurrences.
    #[tokio::test]
    async fn indexing_backfills_memory_vectors_once_memory_representation_is_registered() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        std::fs::write(root.join("main.rs"), "fn parse_config() {}").expect("seed file");

        let ctx = open_ctx(&layout);
        let now_ms = 1_000;
        register_code_raw(&ctx, now_ms).await;
        register_memory(&ctx, now_ms).await;

        ctx.state
            .writer()
            .transaction(move |tx| {
                create_memory_entry(
                    tx,
                    &NewMemoryEntry {
                        memory_id: "mem-1",
                        kind: MemoryKind::Fact,
                        text: "a fact to embed",
                        canonical_key: None,
                        scope_kind: ScopeKind::Global,
                        scope_owner_id: GLOBAL_SCOPE_OWNER_ID,
                        confidence: 0.5,
                        importance: 0.5,
                        valid_from_tree: None,
                        last_verified_tree: None,
                        supersedes_id: None,
                    },
                    now_ms,
                )
            })
            .await
            .expect("seed memory entry tx (infrastructure)")
            .expect("seed memory entry (domain)");

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
        index_worktree(
            &ctx,
            &meta,
            &mut stat_cache,
            &ctx.classifier,
            &scanner,
            now_ms,
        )
        .await
        .expect("index");

        let conn = ctx.cache.open_read().expect("open cache.sqlite for read");
        let memory_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM embedding_cache WHERE subject_kind = 'memory_entry'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert!(memory_rows >= 1, "no memory vector was backfilled");
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
