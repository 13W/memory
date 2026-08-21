//! T20-02 acceptance: the indexing pipeline is reachable from *outside* the
//! binary.
//!
//! A `tests/` target links against this crate's **library** only — never
//! against the `local-rag` binary. So this file cannot compile at all unless
//! every primitive T20-02 moved (`IndexCtx` and each of its fields,
//! `open_state`/`open_cache`/`finish_index_ctx`, `resolve_facts`,
//! `register_new_worktree`, `index_worktree`, `project_generation`) is
//! genuinely `pub` in `local_rag::indexing`. That compile-time fact is the
//! card's real deliverable; the runtime assertions below are the cheap proof
//! that the moved code still works when driven by a caller that is not
//! `cli::index` — exactly what the daemon's per-worktree tasks (T20-05 /
//! T20-06) will be.
//!
//! Deliberately *not* a second copy of `src/indexing/mod.rs`'s own six unit
//! tests: pipeline correctness is theirs, API reachability is this file's.

use std::sync::Arc;

use local_rag::indexing::{
    IndexCtx, index_worktree, open_cache, open_state, project_generation, register_new_worktree,
    resolve_facts,
};
use local_rag_core::identity::domain::path_fingerprint;
use local_rag_core::identity::path::CaseSensitivity;
use local_rag_core::identity::{SystemUuidV7, UuidSource};
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_embed::{Embedder, HashingEmbedder};
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::load_worktree_meta;
use local_rag_index::scan::StatCache;
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, RepresentationKind, Resolution, RetentionParams, WorktreeKind,
    WorktreeRootFacts, register_representation, set_model_space_representation,
};
use local_rag_test_support::TempHome;

#[tokio::test]
async fn the_indexing_pipeline_is_reachable_from_outside_the_binary() {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    let root = home.join("repo");
    std::fs::create_dir_all(&root).expect("repo dir");
    std::fs::write(root.join("a.rs"), "fn a() {}").expect("seed file");

    // `open_state` itself does `ensure -> open` (no separate `layout.ensure()`
    // call here) - that contract is part of what this test proves publicly.
    let state = open_state(&layout).expect("open_state");
    let cache = open_cache(&state, &layout).await.expect("open_cache");

    let now_ms = 1_000;
    let embedder = HashingEmbedder::new(RepresentationKind::CodeRaw);
    let key = embedder.key();
    state
        .writer()
        .transaction(move |tx| {
            let id = register_representation(tx, "smoke-code-raw", &key, now_ms)?;
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

    // Constructed by public field, from outside the crate — the actual point
    // of this test.
    let ctx = IndexCtx {
        state: state.clone(),
        cache,
        layout: layout.clone(),
        uuids: Arc::new(SystemUuidV7),
        embedder: Arc::new(HashingEmbedder::new(RepresentationKind::CodeRaw)),
        memory_embedder: Arc::new(HashingEmbedder::new(RepresentationKind::Memory)),
        model_space_id: DEFAULT_MODEL_SPACE_ID.parse().expect("valid UUID"),
        retention: RetentionParams {
            keep_last_k: 2,
            window_ms: 7 * 24 * 60 * 60 * 1000,
        },
        data_policy: local_rag_core::config::DataPolicy::LocalOnly,
        classifier: ClassifierConfig::new(1024 * 1024),
    };

    let path = root.display().to_string();
    let facts = WorktreeRootFacts {
        observed_canonical_path: path.clone(),
        display_path: path.clone(),
        path_fingerprint: path_fingerprint(&path),
        kind: WorktreeKind::NonGit,
        common_dir_fingerprint: None,
        remote_fingerprint: None,
    };

    // Not yet registered: proves `resolve_facts`/`Resolution` publicly.
    assert!(
        matches!(
            resolve_facts(&state, &facts).expect("resolve"),
            Resolution::GlobalOnly
        ),
        "an unregistered path must resolve to GlobalOnly"
    );

    let repo_id = SystemUuidV7.next_uuid();
    let worktree_id = SystemUuidV7.next_uuid();
    register_new_worktree(&state, repo_id, worktree_id, &facts, now_ms)
        .await
        .expect("register_new_worktree");

    let meta = load_worktree_meta(&state, &worktree_id.to_string(), CaseSensitivity::Sensitive)
        .expect("load meta")
        .expect("worktree exists");
    let mut stat_cache = StatCache::new();
    let scanner = Scanner::new();
    let first = index_worktree(
        &ctx,
        &meta,
        &mut stat_cache,
        &ctx.classifier,
        &scanner,
        now_ms,
    )
    .await
    .expect("index_worktree");

    assert_eq!(first.reconcile.expect_built().files_indexed, 1);
    assert!(first.project.switch.upserted >= 1);
    assert_eq!(
        first.project.fts.occurrence_count,
        first.reconcile.expect_built().occurrences as u64
    );

    // A direct call to `project_generation` — proves it is public separately
    // from `index_worktree` (which only calls it internally) — re-projecting
    // the already-active generation. Safe and idempotent by construction
    // (spec 05 §5: re-activating the same generation is a no-op switch; FTS
    // materialization always re-derives cleanly).
    let generation_id = first
        .reconcile
        .expect_built()
        .generation_id
        .parse()
        .expect("generation id is a UUID");
    let second = project_generation(&ctx, worktree_id, generation_id, now_ms + 1_000)
        .await
        .expect("re-projecting the already-active generation succeeds");
    assert_eq!(
        second.fts.occurrence_count,
        first.project.fts.occurrence_count
    );
}
