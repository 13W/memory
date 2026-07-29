//! T11-02 acceptance tests for `embedding_cache` integrity and eviction (spec
//! 03 §1.2, §4.2, §4.4; 10 §2/§3).
//!
//! Pure coverage (subject-hash golden values, vector codec, the
//! `check_transition`-style corruption checks, the batching seam's dedup
//! logic, and `rows_to_evict`'s pure predicate) lives in `local-rag-core`'s
//! `identity::domain` unit tests and `local-rag-store`'s `cache::embedding`/
//! `eviction` unit tests; these exercise the DB-facing paths end to end —
//! insert/get/delete through [`CacheWriter::transaction`], corruption
//! detection against a real stored row, the batching seam's flush, content
//! sharing across real occurrences via a real `content_blob` join, and a full
//! budget-LRU eviction pass with a real `worktree_projection_state` pin.
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, ids minted
//! from [`uuidv7_from`] with fixed entropy, no network, no wall-clock sleeps.

use local_rag_core::config::StorageConfig;
use local_rag_core::identity::domain::{
    subject_content_blob, subject_memory_entry, subject_occurrence_context,
};
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::code::{
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, UnitKind,
};
use local_rag_store::registry::{
    DEFAULT_MODEL_SPACE_ID, DistanceMetric, ProjectionStateChange, ProjectionStatus,
    RepresentationKey, RepresentationKind, WorktreeKind, allocate_generation, create_repository,
    create_worktree, insert_projection_state, register_representation,
    set_model_space_representation, write_projection_state,
};
use local_rag_store::rusqlite;
use local_rag_store::{
    BatchingLastUsedEmbeddings, CacheDb, EmbeddingCacheMeta, EmbeddingDivergence, EmbeddingKey,
    EvictionParams, LastUsedSinkEmbedding, StateDb, SubjectKind, all_embedding_meta,
    delete_embedding, derive_content_blob, encode_vector_le, flush_last_used_embeddings,
    get_embedding, insert_content_blob, insert_embedding, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, occurrence_id, rows_to_evict,
    run_embedding_cache_eviction, verify_cached_embedding,
};
use local_rag_test_support::TempHome;

const STORE_UUID: &str = "44444444-4444-7444-8444-444444444444";
const NOW: i64 = 1_000_000;

// ---- helpers ----------------------------------------------------------------

/// A temp store with an ensured tree plus opened `state.sqlite` and
/// `cache.sqlite`.
fn open_both() -> (TempHome, StateDb, CacheDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let cache = CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite");
    (home, state, cache)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository and one `active` main worktree; returns the worktree id.
async fn seed_worktree(state: &StateDb, seed: u8) -> String {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(1));
    let (r, w) = (repo, wt.clone());
    state
        .writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, NOW)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, NOW)
        })
        .await
        .expect("seed worktree");
    wt
}

/// Allocate a generation for `worktree_id` (born `building`).
async fn seed_generation(state: &StateDb, worktree_id: &str, generation_id: &str) {
    let (w, g) = (worktree_id.to_string(), generation_id.to_string());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, NOW).map(|_| ()))
        .await
        .expect("allocate generation");
}

/// Insert one `file_revision` + `content_blob` + `parsed_unit` for `content`
/// (real content-addressed `blob_id`). Returns `(file_revision_id, unit_id,
/// blob_id)`.
async fn seed_file_content(
    state: &StateDb,
    file_revision_id: &str,
    unit_id: &str,
    content: &str,
) -> (String, String, String) {
    let derived = derive_content_blob("rust", content);
    let (fr, u, blob, bytes) = (
        file_revision_id.to_string(),
        unit_id.to_string(),
        derived.blob_id.clone(),
        content.as_bytes().to_vec(),
    );
    let len = bytes.len() as i64;
    let (algo, norm) = (derived.algo_version, derived.normalization_version);
    state
        .writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &fr,
                    content_hash: &fr,
                    parser_fingerprint: "test-fp",
                    source_blob: &bytes,
                    compression: local_rag_store::SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: local_rag_store::NewlineStyle::Lf,
                    source_size: len,
                },
                NOW,
            )?;
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &blob,
                    language: "rust",
                    algo_version: algo,
                    normalization_version: norm,
                },
                NOW,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u,
                    file_revision_id: &fr,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: &format!("loc:{u}"),
                    blob_id: &blob,
                    span_start: 0,
                    span_end: len,
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect("seed file content");
    (
        file_revision_id.to_string(),
        unit_id.to_string(),
        derived.blob_id,
    )
}

/// Bind `unit_id` to `normalized_path` as a member+occurrence of `generation_id`.
/// Returns the occurrence id.
async fn seed_occurrence(
    state: &StateDb,
    generation_id: &str,
    normalized_path: &str,
    file_revision_id: &str,
    unit_id: &str,
) -> String {
    let occ = occurrence_id(generation_id, normalized_path, unit_id);
    let (g, path, fr, u, o) = (
        generation_id.to_string(),
        normalized_path.to_string(),
        file_revision_id.to_string(),
        unit_id.to_string(),
        occ.clone(),
    );
    state
        .writer()
        .transaction(move |tx| {
            insert_generation_file(tx, &g, &path, &path, &fr)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &o,
                    generation_id: &g,
                    normalized_path: &path,
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect("seed occurrence");
    occ
}

/// Register a `code_raw` representation and attach it as `required` to
/// [`DEFAULT_MODEL_SPACE_ID`]. Returns the `representation_id`.
async fn seed_code_raw_representation(state: &StateDb, representation_id: &str) -> String {
    let repr_id = representation_id.to_string();
    state
        .writer()
        .transaction(move |tx| {
            let id = register_representation(
                tx,
                &repr_id,
                &RepresentationKey {
                    kind: RepresentationKind::CodeRaw,
                    representation_version: 1,
                    normalization_version: 1,
                    model_id: "test-model".to_string(),
                    dimensions: 3,
                    distance_metric: DistanceMetric::Dot,
                },
                NOW,
            )?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::CodeRaw,
                &id,
                true,
                NOW,
            )?;
            Ok(id)
        })
        .await
        .expect("register representation + attach to default model space")
}

/// Establish `worktree_id`'s active projection tuple as `(generation_id,
/// DEFAULT_MODEL_SPACE_ID)`, via the same write-ahead-then-commit two-step the
/// real switch protocol uses (spec 04 §2).
async fn establish_active_tuple(state: &StateDb, worktree_id: &str, generation_id: &str) {
    let w = worktree_id.to_string();
    state
        .writer()
        .transaction(move |tx| insert_projection_state(tx, &w, NOW))
        .await
        .expect("init projection state");

    let op = uuid(250);
    let (w1, g1, op1) = (
        worktree_id.to_string(),
        generation_id.to_string(),
        op.clone(),
    );
    state
        .writer()
        .transaction(move |tx| {
            write_projection_state(
                tx,
                &w1,
                &ProjectionStateChange {
                    status_to: Some(ProjectionStatus::Updating),
                    target_generation_id: Some(g1),
                    target_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
                    projection_op_id: Some(op1),
                    ..Default::default()
                },
                NOW,
            )
        })
        .await
        .expect("write-ahead")
        .expect("write-ahead legal");

    let (w2, g2, op2) = (worktree_id.to_string(), generation_id.to_string(), op);
    state
        .writer()
        .transaction(move |tx| {
            write_projection_state(
                tx,
                &w2,
                &ProjectionStateChange {
                    status_to: Some(ProjectionStatus::Clean),
                    active_generation_id: Some(g2.clone()),
                    active_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
                    projected_generation_id: Some(g2),
                    projected_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
                    projection_op_id: Some(op2),
                    ..Default::default()
                },
                NOW,
            )
        })
        .await
        .expect("commit")
        .expect("commit legal");
}

fn get_row(cache: &CacheDb, key: &EmbeddingKey) -> Option<local_rag_store::EmbeddingCacheRow> {
    let read = cache.open_read().expect("read conn");
    get_embedding(&read, key).expect("get")
}

async fn put(cache: &CacheDb, key: &EmbeddingKey, dimensions: i64, vector: &[f32], now_ms: i64) {
    let (k, v) = (key.clone(), vector.to_vec());
    cache
        .writer()
        .transaction(move |tx| insert_embedding(tx, &k, dimensions, &v, now_ms))
        .await
        .expect("insert embedding");
}

// ---- hash golden each kind (integration-level round trip) ------------------

#[tokio::test]
async fn hash_golden_each_kind_round_trips_through_the_store() {
    let (_home, _state, cache) = open_both();

    let content_key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-xyz"),
        representation_id: "repr-1".to_string(),
    };
    let context_key = EmbeddingKey {
        subject_kind: SubjectKind::OccurrenceContext,
        subject_hash: subject_occurrence_context(1, b"occ-context-serialization"),
        representation_id: "repr-2".to_string(),
    };
    let memory_key = EmbeddingKey {
        subject_kind: SubjectKind::MemoryEntry,
        subject_hash: subject_memory_entry("memory-1", "remember this"),
        representation_id: "repr-3".to_string(),
    };

    for (key, vector) in [
        (&content_key, vec![1.0f32, 2.0, 3.0]),
        (&context_key, vec![4.0f32, 5.0, 6.0]),
        (&memory_key, vec![7.0f32, 8.0, 9.0]),
    ] {
        put(&cache, key, 3, &vector, NOW).await;
        let row = get_row(&cache, key).expect("row present");
        assert_eq!(row.dimensions, 3);
        assert_eq!(verify_cached_embedding(&row), Ok(()));
        assert_eq!(
            local_rag_store::decode_vector_le(&row.vector_f32).expect("decode"),
            vector
        );
    }
}

// ---- checksum/dimension corrupt row deleted/recomputed ---------------------

#[tokio::test]
async fn checksum_mismatch_is_detected_deleted_and_recomputed() {
    let (_home, _state, cache) = open_both();
    let key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-checksum-test"),
        representation_id: "repr-1".to_string(),
    };
    put(&cache, &key, 2, &[1.0, 2.0], NOW).await;

    // Tamper the stored vector bytes directly (bit-rot stand-in) without
    // touching the checksum column.
    {
        let tampered = encode_vector_le(&[9.0, 9.0]);
        let k = key.clone();
        cache
            .writer()
            .transaction(move |tx| {
                tx.execute(
                    "UPDATE embedding_cache SET vector_f32 = ?4 \
                     WHERE subject_kind = ?1 AND subject_hash = ?2 AND representation_id = ?3",
                    rusqlite::params![
                        k.subject_kind.as_str(),
                        k.subject_hash,
                        k.representation_id,
                        tampered
                    ],
                )
                .map(|_| ())
            })
            .await
            .expect("tamper row");
    }

    let row = get_row(&cache, &key).expect("row present");
    assert_eq!(
        verify_cached_embedding(&row),
        Err(EmbeddingDivergence::ChecksumMismatch)
    );

    // Evict + recompute (regenerate with the correct vector).
    let k = key.clone();
    cache
        .writer()
        .transaction(move |tx| delete_embedding(tx, &k))
        .await
        .expect("evict");
    assert_eq!(get_row(&cache, &key), None, "evicted");

    put(&cache, &key, 2, &[1.0, 2.0], 2000).await;
    let fixed = get_row(&cache, &key).expect("re-cached");
    assert_eq!(verify_cached_embedding(&fixed), Ok(()));
}

#[tokio::test]
async fn dimension_mismatch_is_detected_deleted_and_recomputed() {
    let (_home, _state, cache) = open_both();
    let key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-dimension-test"),
        representation_id: "repr-1".to_string(),
    };
    put(&cache, &key, 2, &[1.0, 2.0], NOW).await;

    // Corrupt the stored `dimensions` value directly (independent of the
    // checksum, which still matches the — now mis-declared — vector bytes).
    {
        let k = key.clone();
        cache
            .writer()
            .transaction(move |tx| {
                tx.execute(
                    "UPDATE embedding_cache SET dimensions = 99 \
                     WHERE subject_kind = ?1 AND subject_hash = ?2 AND representation_id = ?3",
                    rusqlite::params![k.subject_kind.as_str(), k.subject_hash, k.representation_id],
                )
                .map(|_| ())
            })
            .await
            .expect("corrupt dimensions");
    }

    let row = get_row(&cache, &key).expect("row present");
    assert_eq!(
        verify_cached_embedding(&row),
        Err(EmbeddingDivergence::DimensionMismatch)
    );

    let k = key.clone();
    cache
        .writer()
        .transaction(move |tx| delete_embedding(tx, &k))
        .await
        .expect("evict");
    put(&cache, &key, 2, &[1.0, 2.0], 2000).await;
    let fixed = get_row(&cache, &key).expect("re-cached");
    assert_eq!(verify_cached_embedding(&fixed), Ok(()));
}

// ---- content shares across occurrences while context does not -------------

#[tokio::test]
async fn content_shares_across_occurrences_while_context_does_not() {
    let (_home, state, _cache) = open_both();
    let wt = seed_worktree(&state, 1).await;
    let gen_id = uuid(10);
    seed_generation(&state, &wt, &gen_id).await;

    // Two occurrences (distinct paths) whose units point at the SAME content —
    // real structural sharing (spec 06 §2): one file_revision/content_blob,
    // two parsed_units (distinct spans not needed for this test), two paths.
    let (fr, unit_a, blob) =
        seed_file_content(&state, &uuid(11), &uuid(12), "fn shared() {}\n").await;
    seed_occurrence(&state, &gen_id, "a.rs", &fr, &unit_a).await;
    // A second occurrence over a DIFFERENT unit but the SAME blob_id (as if a
    // second file had byte-identical content — content_blob is reused).
    let unit_b = uuid(13);
    {
        let (u, f, b) = (unit_b.clone(), fr.clone(), blob.clone());
        state
            .writer()
            .transaction(move |tx| {
                insert_parsed_unit(
                    tx,
                    &NewParsedUnit {
                        unit_id: &u,
                        file_revision_id: &f,
                        unit_kind: UnitKind::Symbol,
                        syntax_locator: &format!("loc:{u}"),
                        blob_id: &b,
                        span_start: 0,
                        span_end: 1,
                        local_name: None,
                        kind: None,
                        parent_unit_id: None,
                    },
                )
            })
            .await
            .expect("second parsed_unit, same blob");
    }
    seed_occurrence(&state, &gen_id, "b.rs", &fr, &unit_b).await;

    let read = state.open_read().expect("read conn");
    let pairs = local_rag_store::content_blob_ids_for_generation(&read, &gen_id).expect("join");
    assert_eq!(pairs.len(), 2, "two occurrences");
    let blob_ids: Vec<&str> = pairs.iter().map(|(_, b)| b.as_str()).collect();
    assert_eq!(
        blob_ids[0], blob_ids[1],
        "both occurrences share one blob_id"
    );
    // -> identical subject hash, identical embedding_cache row.
    assert_eq!(
        subject_content_blob(blob_ids[0]),
        subject_content_blob(blob_ids[1])
    );

    // Context does not share: two synthetic (opaque, [OPEN]-format-agnostic)
    // context serializations produce distinct subject hashes.
    let ctx_a = subject_occurrence_context(1, b"occurrence-a-context");
    let ctx_b = subject_occurrence_context(1, b"occurrence-b-context");
    assert_ne!(ctx_a, ctx_b, "distinct contexts never share a subject hash");
}

// ---- eviction honors pins/budget --------------------------------------------

#[tokio::test]
async fn eviction_honors_pins_and_budget() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 20).await;
    let gen_id = uuid(30);
    seed_generation(&state, &wt, &gen_id).await;
    let (fr, unit, blob) =
        seed_file_content(&state, &uuid(31), &uuid(32), "fn pinned() {}\n").await;
    seed_occurrence(&state, &gen_id, "pinned.rs", &fr, &unit).await;

    let repr_id = seed_code_raw_representation(&state, &uuid(33)).await;
    establish_active_tuple(&state, &wt, &gen_id).await;

    // The pinned row: the active generation's only occurrence, code_raw kind,
    // the representation registered as required for the default model space.
    let pinned_key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob(&blob),
        representation_id: repr_id,
    };
    put(&cache, &pinned_key, 3, &[1.0, 2.0, 3.0], 100).await; // oldest last_used_at

    // Two unrelated, unpinned rows — older and newer than the pinned one.
    let stale_key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("unrelated-blob-stale"),
        representation_id: "unrelated-repr".to_string(),
    };
    put(&cache, &stale_key, 3, &[4.0, 5.0, 6.0], 50).await; // oldest overall
    let fresh_key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("unrelated-blob-fresh"),
        representation_id: "unrelated-repr".to_string(),
    };
    put(&cache, &fresh_key, 3, &[7.0, 8.0, 9.0], 500).await; // newest

    let state_read = state.open_read().expect("state read conn");
    let params = EvictionParams::from_storage_config(&StorageConfig {
        embedding_cache_budget_mb: 0,
        ..StorageConfig::default()
    });
    // A near-zero budget (each vector is 3*4=12 bytes; total = 36) forces
    // eviction of everything unpinned, oldest first.
    let dry = run_embedding_cache_eviction(&cache, &state_read, &params, NOW, true)
        .await
        .expect("dry run");
    assert!(dry.dry_run);
    assert!(
        dry.evicted.contains(&stale_key) && dry.evicted.contains(&fresh_key),
        "dry run reports both unrelated rows as evictable, got {:?}",
        dry.evicted
    );
    assert!(
        !dry.evicted.contains(&pinned_key),
        "the pinned row is never listed for eviction"
    );
    // Dry run mutates nothing.
    assert!(get_row(&cache, &stale_key).is_some());

    let real = run_embedding_cache_eviction(&cache, &state_read, &params, NOW, false)
        .await
        .expect("real eviction");
    assert!(!real.dry_run);
    assert_eq!(
        get_row(&cache, &pinned_key).map(|_| ()),
        Some(()),
        "pinned row survives"
    );
    assert_eq!(
        get_row(&cache, &stale_key),
        None,
        "stale unpinned row evicted"
    );
    assert_eq!(
        get_row(&cache, &fresh_key),
        None,
        "fresh unpinned row also evicted (still over budget)"
    );

    // Re-running finds nothing left to evict.
    let idempotent = run_embedding_cache_eviction(&cache, &state_read, &params, NOW, false)
        .await
        .expect("idempotent re-run");
    assert!(idempotent.evicted.is_empty());
}

/// A generous budget leaves everything in place, pinned or not.
#[tokio::test]
async fn eviction_is_a_noop_under_budget() {
    let (_home, state, cache) = open_both();
    let key_a = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-a"),
        representation_id: "repr-1".to_string(),
    };
    let key_b = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-b"),
        representation_id: "repr-1".to_string(),
    };
    put(&cache, &key_a, 2, &[1.0, 2.0], 100).await;
    put(&cache, &key_b, 2, &[3.0, 4.0], 200).await;

    let state_read = state.open_read().expect("read conn");
    let params = EvictionParams::from_storage_config(&StorageConfig::default()); // 2048 MiB
    let report = run_embedding_cache_eviction(&cache, &state_read, &params, NOW, false)
        .await
        .expect("eviction");
    assert!(report.evicted.is_empty());
    assert!(get_row(&cache, &key_a).is_some());
    assert!(get_row(&cache, &key_b).is_some());
}

// ---- cache loss safe ---------------------------------------------------------

#[tokio::test]
async fn cache_loss_is_safe() {
    let (_home, _state, cache) = open_both();
    let key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-loss-test"),
        representation_id: "repr-1".to_string(),
    };
    put(&cache, &key, 2, &[1.0, 2.0], NOW).await;
    assert!(get_row(&cache, &key).is_some());

    // Simulate total loss: delete every row directly (stands in for losing
    // the whole cache file — the table itself is never dropped by product
    // code, only rows are evicted; `CacheDb::open`'s own drop-and-recreate
    // path is already covered by `tests/cache.rs`).
    cache
        .writer()
        .transaction(move |tx| tx.execute("DELETE FROM embedding_cache", []).map(|_| ()))
        .await
        .expect("simulate total loss");

    // No panic, no error — a clean absence.
    assert_eq!(get_row(&cache, &key), None);
    let read = cache.open_read().expect("read conn");
    assert_eq!(
        all_embedding_meta(&read).expect("meta scan"),
        Vec::<EmbeddingCacheMeta>::new()
    );

    // A fresh insert afterward succeeds normally — loss is recoverable.
    put(&cache, &key, 2, &[1.0, 2.0], 2000).await;
    assert!(get_row(&cache, &key).is_some());
}

// ---- batched last_used_at seam ----------------------------------------------

#[tokio::test]
async fn last_used_batching_seam_flushes() {
    let (_home, _state, cache) = open_both();
    let a = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-a"),
        representation_id: "repr-1".to_string(),
    };
    let b = EmbeddingKey {
        subject_kind: SubjectKind::MemoryEntry,
        subject_hash: subject_memory_entry("memory-1", "text"),
        representation_id: "repr-2".to_string(),
    };
    put(&cache, &a, 2, &[1.0, 2.0], 1000).await;
    put(&cache, &b, 2, &[3.0, 4.0], 1000).await;

    let sink = BatchingLastUsedEmbeddings::new();
    sink.record_used(&a, 5000);
    sink.record_used(&a, 4000); // earlier — dedups to 5000
    sink.record_used(&b, 6000);
    let evicted_key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("never-cached"),
        representation_id: "repr-1".to_string(),
    };
    sink.record_used(&evicted_key, 7000); // not in cache
    assert_eq!(sink.len(), 3);

    let updates = sink.drain();
    assert!(sink.is_empty(), "drain clears the buffer");
    let applied = cache
        .writer()
        .transaction(move |tx| flush_last_used_embeddings(tx, &updates))
        .await
        .expect("flush");
    assert_eq!(applied, 2, "only the two present rows are updated");

    let row_a = get_row(&cache, &a).expect("a present");
    assert_eq!(row_a.last_used_at, 5000, "latest recorded timestamp wins");
    assert_eq!(row_a.created_at, 1000, "created_at is untouched");
    let row_b = get_row(&cache, &b).expect("b present");
    assert_eq!(row_b.last_used_at, 6000);
}

// ---- pure rows_to_evict sanity against real meta rows -----------------------

#[tokio::test]
async fn all_embedding_meta_reflects_real_rows_for_pure_eviction_planning() {
    let (_home, _state, cache) = open_both();
    let a = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-a"),
        representation_id: "repr-1".to_string(),
    };
    put(&cache, &a, 3, &[1.0, 2.0, 3.0], 42).await;

    let read = cache.open_read().expect("read conn");
    let meta = all_embedding_meta(&read).expect("meta scan");
    assert_eq!(meta.len(), 1);
    assert_eq!(meta[0].key, a);
    assert_eq!(meta[0].byte_size, 12);
    assert_eq!(meta[0].last_used_at, 42);

    // Feeding this straight into the pure predicate behaves as expected.
    let evict = rows_to_evict(&meta, &std::collections::BTreeSet::new(), 0);
    assert_eq!(evict, vec![a]);
}

// ---- embeddings_for_subject_kind (T14-08) -----------------------------------

#[tokio::test]
async fn embeddings_for_subject_kind_filters_by_kind_and_representation() {
    let (_home, _state, cache) = open_both();

    let memory_a = EmbeddingKey {
        subject_kind: SubjectKind::MemoryEntry,
        subject_hash: subject_memory_entry("memory-a", "text a"),
        representation_id: "memory-repr".to_string(),
    };
    let memory_b = EmbeddingKey {
        subject_kind: SubjectKind::MemoryEntry,
        subject_hash: subject_memory_entry("memory-b", "text b"),
        representation_id: "memory-repr".to_string(),
    };
    // Same subject kind, different representation — must not leak in.
    let memory_other_repr = EmbeddingKey {
        subject_kind: SubjectKind::MemoryEntry,
        subject_hash: subject_memory_entry("memory-c", "text c"),
        representation_id: "other-repr".to_string(),
    };
    // Same representation, different subject kind — must not leak in either.
    let content_same_repr = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob("blob-shares-repr-id"),
        representation_id: "memory-repr".to_string(),
    };

    put(&cache, &memory_a, 2, &[1.0, 0.0], NOW).await;
    put(&cache, &memory_b, 2, &[0.0, 1.0], NOW).await;
    put(&cache, &memory_other_repr, 2, &[9.0, 9.0], NOW).await;
    put(&cache, &content_same_repr, 2, &[8.0, 8.0], NOW).await;

    let read = cache.open_read().expect("read conn");
    let rows = local_rag_store::embeddings_for_subject_kind(
        &read,
        SubjectKind::MemoryEntry,
        "memory-repr",
        100,
    )
    .expect("bulk scan");

    let hashes: Vec<&str> = rows.iter().map(|r| r.subject_hash.as_str()).collect();
    let mut expected = vec![
        memory_a.subject_hash.as_str(),
        memory_b.subject_hash.as_str(),
    ];
    expected.sort_unstable();
    assert_eq!(
        hashes, expected,
        "deterministic subject_hash-ascending order, other kind/representation excluded"
    );

    for row in &rows {
        assert_eq!(verify_cached_embedding(&row.row), Ok(()));
    }
}

#[tokio::test]
async fn embeddings_for_subject_kind_respects_the_limit() {
    let (_home, _state, cache) = open_both();

    for seed in 0..5u8 {
        let key = EmbeddingKey {
            subject_kind: SubjectKind::MemoryEntry,
            subject_hash: subject_memory_entry(&format!("memory-{seed}"), "t"),
            representation_id: "memory-repr".to_string(),
        };
        put(&cache, &key, 1, &[seed as f32], NOW).await;
    }

    let read = cache.open_read().expect("read conn");
    let rows = local_rag_store::embeddings_for_subject_kind(
        &read,
        SubjectKind::MemoryEntry,
        "memory-repr",
        3,
    )
    .expect("bulk scan");
    assert_eq!(rows.len(), 3, "bounded by the caller-supplied limit");
}
