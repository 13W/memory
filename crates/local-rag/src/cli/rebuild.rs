//! `local-rag rebuild --worktree <id> [--fts] [--dense]` (spec 11 §6, spec
//! 05 §7).
//!
//! Neither leg needs a live embedder: `--fts` re-derives the FTS view from
//! already-indexed content, and `--dense` (`local_rag_projection::
//! force_rebuild`, T15-07) reads vectors already sitting in `embedding_cache`
//! — the same `CacheVectorSource` seam `local_rag::indexing::project_generation`
//! uses, never re-running `run_backfill`. A model space with no registered
//! `code_raw` representation still refuses (`ShardParams` has nowhere to
//! come from), but that refusal is about the registry, not about whether
//! `ORT_DYLIB_PATH`/weights are present on this machine.

use std::process::ExitCode;

use local_rag::indexing::{open_cache, open_state};
use local_rag_core::identity::{Uuid, UuidSource};
use local_rag_projection::{
    BruteForceProjectionStore, CacheVectorSource, ModelSwitchError, force_rebuild, shard_dir,
};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, current_generation, materialize_fts, projection_state,
};

use super::{EXIT_USAGE, block_on, fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

#[derive(Debug, clap::Args)]
pub struct RebuildArgs {
    #[arg(long)]
    worktree: String,
    /// Re-derive the FTS view from already-indexed content.
    #[arg(long)]
    fts: bool,
    /// Re-derive the dense projection from already-embedded vectors.
    #[arg(long)]
    dense: bool,
}

pub fn run(args: RebuildArgs) -> ExitCode {
    let RebuildArgs {
        worktree,
        fts,
        dense,
    } = args;
    if !fts && !dense {
        eprintln!("{BIN} rebuild: at least one of --fts or --dense is required");
        return ExitCode::from(EXIT_USAGE);
    }
    let worktree_id: Uuid = match worktree.parse() {
        Ok(id) => id,
        Err(_) => return fail(BIN, &format!("{worktree:?} is not a valid worktree id")),
    };

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };
    let state = match open_state(&layout) {
        Ok(s) => s,
        Err(e) => return fail(BIN, &e),
    };

    block_on(async {
        let now_ms = system_now_ms();
        let mut report = Vec::new();

        if fts {
            match run_rebuild_fts(&state, &layout, worktree_id, now_ms).await {
                Ok(occurrences) => report.push(format!("fts: {occurrences} occurrences")),
                Err(e) => return fail(BIN, &e),
            }
        }
        if dense {
            match run_rebuild_dense(&state, &layout, worktree_id, now_ms).await {
                Ok(Some(outcome)) => {
                    report.push(format!(
                        "dense: {} points, op {}",
                        outcome.point_count, outcome.projection_op_id
                    ));
                }
                Ok(None) => {
                    return fail(
                        BIN,
                        "no active generation for this worktree yet; run `local-rag index`/`reindex` first",
                    );
                }
                Err(e) => return fail(BIN, &e),
            }
        }

        println!("{BIN}: rebuilt {} ({})", worktree_id, report.join(", "));
        ExitCode::SUCCESS
    })
}

async fn run_rebuild_fts(
    state: &local_rag_store::StateDb,
    layout: &local_rag_core::paths::StoreLayout,
    worktree_id: Uuid,
    now_ms: i64,
) -> Result<u64, String> {
    let generation_id = {
        let conn = state
            .open_read()
            .map_err(|e| format!("could not open state.sqlite: {e}"))?;
        current_generation(&conn, &worktree_id.to_string())
            .map_err(|e| format!("could not read the current generation: {e}"))?
    };
    let Some(generation_id) = generation_id else {
        return Err(
            "no active generation for this worktree yet; run `local-rag index`/`reindex` first"
                .to_string(),
        );
    };
    let cache = open_cache(state, layout).await?;
    materialize_fts(
        state,
        &cache,
        &worktree_id.to_string(),
        &generation_id,
        now_ms,
    )
    .await
    .map(|outcome| outcome.occurrence_count)
    .map_err(|e| format!("FTS rebuild failed: {e}"))
}

async fn run_rebuild_dense(
    state: &std::sync::Arc<local_rag_store::StateDb>,
    layout: &local_rag_core::paths::StoreLayout,
    worktree_id: Uuid,
    now_ms: i64,
) -> Result<Option<local_rag_projection::RebuildOutcome>, String> {
    let model_space_id: Uuid = DEFAULT_MODEL_SPACE_ID
        .parse()
        .expect("DEFAULT_MODEL_SPACE_ID is a valid UUID");

    let (active_generation_id, active_model_space_id) = {
        let conn = state
            .open_read()
            .map_err(|e| format!("could not open state.sqlite: {e}"))?;
        let row = projection_state(&conn, &worktree_id.to_string())
            .map_err(|e| format!("could not read projection state: {e}"))?;
        match row.and_then(|r| r.active_generation_id.zip(r.active_model_space_id)) {
            Some(pair) => pair,
            None => return Ok(None),
        }
    };
    let active_generation_id: Uuid = active_generation_id
        .parse()
        .map_err(|_| "internal error: active generation id is not a UUID".to_string())?;
    let active_model_space_id: Uuid = active_model_space_id
        .parse()
        .map_err(|_| "internal error: active model space id is not a UUID".to_string())?;

    let params = {
        let conn = state
            .open_read()
            .map_err(|e| format!("could not open state.sqlite: {e}"))?;
        local_rag_projection::params_for_model_space(&conn, &model_space_id).map_err(
            |e| match e {
                ModelSwitchError::NoShardParams { model_space_id } => format!(
                    "model space {model_space_id} has no code_raw representation \
                     registered yet; run `local-rag init --download-models` first"
                ),
                other => other.to_string(),
            },
        )?
    };

    let cache = open_cache(state, layout).await?;
    let uuids = local_rag_core::identity::SystemUuidV7;
    let vectors = {
        let read = state
            .open_read()
            .map_err(|e| format!("could not open state.sqlite: {e}"))?;
        CacheVectorSource::new(
            state,
            &cache,
            &read,
            &active_generation_id,
            &active_model_space_id,
        )
        .map_err(|e| format!("could not build a vector source: {e}"))?
    };

    force_rebuild(
        state,
        &BruteForceProjectionStore::new(),
        &shard_dir(layout, &worktree_id, &model_space_id),
        &layout.quarantine_dir(),
        params,
        worktree_id,
        &vectors,
        &uuids as &(dyn UuidSource + Send + Sync),
        now_ms,
    )
    .await
    .map_err(|e| match e {
        local_rag_projection::RebuildError::MissingVector { .. } => {
            format!("{e}; run `local-rag reindex`, then repeat `rebuild --dense`")
        }
        other => other.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use local_rag_core::identity::domain::path_fingerprint;
    use local_rag_core::identity::path::CaseSensitivity;
    use local_rag_core::identity::uuidv7_from;
    use local_rag_core::paths::StoreLayout;
    use local_rag_core::redaction::Scanner;
    use local_rag_embed::HashingEmbedder;
    use local_rag_index::classify::ClassifierConfig;
    use local_rag_index::reconcile::load_worktree_meta;
    use local_rag_index::scan::StatCache;
    use local_rag_store::{
        RepresentationKind, RetentionParams, StateDb, WorktreeKind, WorktreeRootFacts,
        register_representation, set_model_space_representation,
    };
    use local_rag_test_support::TempHome;

    use local_rag::indexing::{IndexCtx, index_worktree, register_new_worktree};

    use super::*;

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
            uuidv7_from(9_500_000 + n, [0x53; 10])
        }
    }

    /// Builds `ctx.cache` through the real `open_cache` (`ensure_store_instance_uuid`),
    /// not a hardcoded literal: `run_rebuild_fts`/`run_rebuild_dense` each open
    /// their *own* fresh `CacheDb` handle against the same `cache.sqlite`, and
    /// `CacheDb::open` **rebuilds (wipes) an incompatibly-bound cache**
    /// (`cache/mod.rs`'s own doc) — a literal here would silently discard
    /// every embedding `index_once` just wrote before the rebuild functions
    /// ever got a chance to read it back.
    async fn open_ctx(layout: &StoreLayout) -> IndexCtx {
        let state = Arc::new(StateDb::open(layout.state_db()).expect("open state.sqlite"));
        let cache = open_cache(&state, layout).await.expect("open cache.sqlite");
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

    async fn seeded_worktree(ctx: &IndexCtx, root: &std::path::Path, now_ms: i64) -> Uuid {
        let repo_id = ctx.uuids.next_uuid();
        let worktree_id = ctx.uuids.next_uuid();
        let facts = facts_for(root);
        register_new_worktree(&ctx.state, repo_id, worktree_id, &facts, now_ms)
            .await
            .expect("register worktree");
        worktree_id
    }

    async fn index_once(ctx: &IndexCtx, worktree_id: Uuid, now_ms: i64) {
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
            ctx,
            &meta,
            &mut stat_cache,
            &ctx.classifier,
            &scanner,
            now_ms,
        )
        .await
        .expect("index");
    }

    #[tokio::test]
    async fn rebuild_fts_reprojects_the_fts_view() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        std::fs::write(root.join("a.rs"), "fn a() {}").expect("seed file");

        let ctx = open_ctx(&layout).await;
        let now_ms = 1_000;
        register_code_raw(&ctx, now_ms).await;
        let worktree_id = seeded_worktree(&ctx, &root, now_ms).await;
        index_once(&ctx, worktree_id, now_ms).await;

        let occurrences = run_rebuild_fts(&ctx.state, &layout, worktree_id, now_ms + 1_000)
            .await
            .expect("rebuild fts");
        // One file-level occurrence plus one for the `a` symbol.
        assert_eq!(occurrences, 2);
    }

    #[tokio::test]
    async fn rebuild_dense_forces_a_rebuild_even_when_the_shard_is_already_valid() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");
        std::fs::write(root.join("a.rs"), "fn a() {}").expect("seed file");

        let ctx = open_ctx(&layout).await;
        let now_ms = 1_000;
        register_code_raw(&ctx, now_ms).await;
        let worktree_id = seeded_worktree(&ctx, &root, now_ms).await;
        index_once(&ctx, worktree_id, now_ms).await;

        let outcome = run_rebuild_dense(&ctx.state, &layout, worktree_id, now_ms + 1_000)
            .await
            .expect("rebuild dense")
            .expect("an active generation exists");
        assert_eq!(outcome.point_count, 2);
    }

    #[tokio::test]
    async fn rebuild_fts_without_an_active_generation_reports_a_hint() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");

        let ctx = open_ctx(&layout).await;
        let now_ms = 1_000;
        let worktree_id = seeded_worktree(&ctx, &root, now_ms).await;

        let err = run_rebuild_fts(&ctx.state, &layout, worktree_id, now_ms)
            .await
            .expect_err("never indexed");
        assert!(err.contains("local-rag index"), "{err}");
    }

    #[tokio::test]
    async fn rebuild_dense_without_an_active_generation_returns_none() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let root = home.join("repo");
        std::fs::create_dir_all(&root).expect("repo dir");

        let ctx = open_ctx(&layout).await;
        let now_ms = 1_000;
        let worktree_id = seeded_worktree(&ctx, &root, now_ms).await;

        let outcome = run_rebuild_dense(&ctx.state, &layout, worktree_id, now_ms)
            .await
            .expect("no error when nothing to rebuild");
        assert!(outcome.is_none());
    }
}
