//! T09-02 acceptance tests for the ref-counted shard LRU manager (spec 02 §5
//! L3, 05 §2/§8), mapped 1:1 to the task card's five required scenarios.
//!
//! Fixture helpers mirror `crates/projection/tests/rebuild.rs` (integration
//! test binaries can't share code without a `mod` file; this duplicates that
//! file's small helper set, matching the existing `switch.rs`/`rebuild.rs`
//! convention). All tests are deterministic: no network, no `$HOME`
//! dependency (isolated `TempHome`), and no wall-clock sleeps — concurrency
//! is proven with `tokio::spawn`/`spawn_blocking`/`yield_now` and std
//! channels, mirroring `crates/store/tests/lock.rs`'s idiom.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    AcquireError, FakeProjectionStore, ProjectionStore, RepresentationKind, ShardHandle,
    ShardManager, ShardParams, VectorSource, switch,
};
use local_rag_store::{
    DEFAULT_MODEL_SPACE_ID, GenerationState, NewContentBlob, NewFileRevision, NewOccurrence,
    NewParsedUnit, NewlineStyle, SourceCompression, StateDb, UnitKind, WorktreeKind,
    allocate_generation, create_repository, create_worktree, insert_content_blob,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    insert_projection_state, occurrence_id, transition_generation,
};
use local_rag_test_support::TempHome;

const DIMS: usize = 3;

fn params() -> ShardParams {
    ShardParams::with_dimensions(DIMS)
}

fn default_model_space() -> Uuid {
    DEFAULT_MODEL_SPACE_ID
        .parse()
        .expect("default model space id parses")
}

struct SeqUuidV7 {
    counter: AtomicU64,
}

impl SeqUuidV7 {
    fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(4_000_000 + n, [0x22; 10])
    }
}

/// A test-only [`VectorSource`]: always returns a fixed `DIMS`-wide vector.
/// A stateless unit struct (unlike `rebuild.rs`/`switch.rs`'s own
/// `FakeVectors`, which tracks blocked keys and call history this file's
/// tests never need) — trivially `Sync`, required by `switch`/
/// `open_and_validate`'s `&(dyn VectorSource + Send + Sync)` parameter.
struct FakeVectors;

impl VectorSource for FakeVectors {
    fn vector(&self, _occurrence_id: &str, _kind: RepresentationKind) -> Option<Vec<f32>> {
        Some(vec![1.0, 0.0, 0.0])
    }
}

/// A [`ProjectionStore`] wrapping [`FakeProjectionStore`] that counts every
/// physical `open()` call.
struct CountingStore {
    inner: FakeProjectionStore,
    opens: AtomicU64,
}

impl CountingStore {
    fn new() -> Self {
        Self {
            inner: FakeProjectionStore::new(),
            opens: AtomicU64::new(0),
        }
    }

    fn open_count(&self) -> u64 {
        self.opens.load(Ordering::SeqCst)
    }
}

impl ProjectionStore for CountingStore {
    fn open(
        &self,
        dir: &Path,
        params: ShardParams,
    ) -> local_rag_projection::Result<Box<dyn ShardHandle>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.inner.open(dir, params)
    }
}

fn open_state() -> (TempHome, StoreLayout, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, layout, db)
}

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(2000, rand)
}

async fn worktree(db: &StateDb, seed: u8) -> Uuid {
    let repo = uuid(seed).to_string();
    let wt = uuid(seed.wrapping_add(100));
    let (r, w) = (repo, wt.to_string());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)
        })
        .await
        .expect("create repo + worktree");
    wt
}

async fn init_projection(db: &StateDb, worktree_id: &Uuid) {
    let w = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| insert_projection_state(tx, &w, 1000))
        .await
        .expect("init projection state");
    // The default model space must declare what it requires now that the
    // expected point set joins the registry (T11-05).
    register_code_representations(db, &default_model_space()).await;
}

async fn allocate_ready(db: &StateDb, worktree_id: &Uuid, gen_seed: u8) -> Uuid {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.to_string());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 1000).map(|_| ()))
        .await
        .expect("allocate generation");
    let g2 = genr.to_string();
    db.writer()
        .transaction(move |tx| transition_generation(tx, &g2, GenerationState::ProjectionReady))
        .await
        .expect("transition tx (infra)")
        .expect("building -> projection_ready is legal");
    genr
}

async fn seed_occurrence(db: &StateDb, generation_id: &Uuid, seed: u8, path: &str) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let blob = uuid(seed.wrapping_add(30)).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = occurrence_id(&gen_str, path, &unit);
    let (rev, b, u, g, p, occ2) = (revision, blob, unit, gen_str, path.to_string(), occ.clone());
    db.writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &rev,
                    content_hash: &rev,
                    parser_fingerprint: "fp",
                    source_blob: b"hello\n",
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: 6,
                },
                1000,
            )?;
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &b,
                    language: "rust",
                    algo_version: 1,
                    normalization_version: 1,
                },
                1000,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u,
                    file_revision_id: &rev,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:main",
                    blob_id: &b,
                    span_start: 0,
                    span_end: 6,
                    local_name: Some("main"),
                    kind: Some("fn"),
                    parent_unit_id: None,
                },
            )?;
            insert_generation_file(tx, &g, &p, &p, &rev)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ2,
                    generation_id: &g,
                    normalized_path: &p,
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

/// Establish a real, converged `active` tuple + shard via one `switch()`:
/// worktree, one occurrence, model space default.
async fn established(
    db: &StateDb,
    layout: &StoreLayout,
    seed: u8,
) -> (Uuid, Uuid, std::path::PathBuf) {
    let wt = worktree(db, seed).await;
    init_projection(db, &wt).await;
    let gen_a = allocate_ready(db, &wt, seed.wrapping_add(1)).await;
    seed_occurrence(db, &gen_a, seed.wrapping_add(2), "a.rs").await;
    let ms = default_model_space();
    let shard_dir = layout.projection_shard(&wt.to_string());

    switch(
        db,
        &FakeProjectionStore::new(),
        &shard_dir,
        params(),
        wt,
        gen_a,
        ms,
        &FakeVectors,
        &SeqUuidV7::new(),
        1000,
    )
    .await
    .expect("establish active tuple via switch");

    (wt, gen_a, shard_dir)
}

/// **"LRU order/cap"**: eviction removes the least-recently-used entry once
/// `max_open_shards` is exceeded, and a re-`acquire`d entry's bumped recency
/// changes who gets evicted next — proving real recency order, not FIFO.
/// Register the two code representations (`code_raw`, `code_context`) as
/// `required` for `model_space_id`.
///
/// T11-05 replaced `expected::REQUIRED_REPRESENTATION_KINDS`'s hardcoded pair
/// with a real `model_space_representation` join, so a fixture's model space now
/// has to declare what it requires. Registering exactly that pair keeps every
/// pre-existing expectation in this file (2 points per occurrence) unchanged.
async fn register_code_representations(db: &StateDb, model_space_id: &Uuid) {
    let space = model_space_id.to_string();
    db.writer()
        .transaction(move |tx| {
            for (i, kind) in [
                local_rag_store::RepresentationKind::CodeRaw,
                local_rag_store::RepresentationKind::CodeContext,
            ]
            .into_iter()
            .enumerate()
            {
                let representation_id = format!("{space}-repr-{i}");
                let id = local_rag_store::register_representation(
                    tx,
                    &representation_id,
                    &local_rag_store::RepresentationKey {
                        kind,
                        representation_version: 1,
                        normalization_version: 1,
                        model_id: format!("test-model-{space}"),
                        dimensions: DIMS as u32,
                        distance_metric: local_rag_store::DistanceMetric::Cosine,
                    },
                    1000,
                )?;
                local_rag_store::set_model_space_representation(tx, &space, kind, &id, true, 1000)?;
            }
            Ok(())
        })
        .await
        .expect("register code representations");
}

#[tokio::test]
async fn lru_evicts_least_recently_used_once_over_capacity() {
    let (_home, layout, db) = open_state();
    let db = Arc::new(db);
    let wt1 = worktree(&db, 10).await;
    init_projection(&db, &wt1).await;
    let wt2 = worktree(&db, 11).await;
    init_projection(&db, &wt2).await;
    let wt3 = worktree(&db, 12).await;
    init_projection(&db, &wt3).await;
    let wt4 = worktree(&db, 13).await;
    init_projection(&db, &wt4).await;

    let manager = ShardManager::new(
        db,
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(FakeVectors),
        Arc::new(SeqUuidV7::new()),
        2,
    );

    manager.acquire(wt1, 1000).await.expect("acquire wt1");
    manager.acquire(wt2, 1000).await.expect("acquire wt2");
    manager.acquire(wt3, 1000).await.expect("acquire wt3");

    assert_eq!(manager.open_count(), 2);
    assert!(!manager.is_cached(wt1), "wt1 is the least-recently-used");
    assert!(manager.is_cached(wt2));
    assert!(manager.is_cached(wt3));

    // Bump wt2's recency, then acquire wt4: wt3 (untouched since) must be
    // evicted instead of wt2 — proves real recency order, not FIFO.
    manager.acquire(wt2, 1000).await.expect("re-acquire wt2");
    manager.acquire(wt4, 1000).await.expect("acquire wt4");

    assert!(manager.is_cached(wt2), "wt2 was touched most recently");
    assert!(!manager.is_cached(wt3), "wt3 was untouched since, evicted");
    assert!(manager.is_cached(wt4));
}

/// **"concurrent same-key open once"**: two concurrent `acquire`s of the same
/// worktree collapse into exactly one physical `store.open()`, and both
/// callers receive the identical handle.
#[tokio::test]
async fn concurrent_acquire_of_same_worktree_opens_once() {
    let (_home, layout, db) = open_state();
    let db = Arc::new(db);
    let wt = worktree(&db, 20).await;
    init_projection(&db, &wt).await;

    let store = Arc::new(CountingStore::new());
    let manager = Arc::new(ShardManager::new(
        db,
        store.clone(),
        layout,
        params(),
        Arc::new(FakeVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));

    let m1 = manager.clone();
    let m2 = manager.clone();
    let t1 = tokio::spawn(async move { m1.acquire(wt, 1000).await });
    let t2 = tokio::spawn(async move { m2.acquire(wt, 1000).await });
    let h1 = t1.await.expect("join 1").expect("acquire 1");
    let h2 = t2.await.expect("join 2").expect("acquire 2");

    assert!(Arc::ptr_eq(&h1, &h2), "both callers got the same handle");
    assert_eq!(store.open_count(), 1, "exactly one physical open");
}

/// **"in-use eviction deferred"**: a handle held past the cap is never
/// evicted; once released, it becomes evictable again.
#[tokio::test]
async fn in_use_handle_is_never_evicted() {
    let (_home, layout, db) = open_state();
    let db = Arc::new(db);
    let wt_a = worktree(&db, 30).await;
    init_projection(&db, &wt_a).await;
    let wt_b = worktree(&db, 31).await;
    init_projection(&db, &wt_b).await;

    let manager = ShardManager::new(
        db,
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(FakeVectors),
        Arc::new(SeqUuidV7::new()),
        1,
    );

    let handle_a = manager.acquire(wt_a, 1000).await.expect("acquire A");
    manager.acquire(wt_b, 1000).await.expect("acquire B");

    assert!(
        manager.is_cached(wt_a),
        "A is in use; eviction deferred rather than forced"
    );
    assert!(
        manager.is_cached(wt_b),
        "B is present (transiently over cap)"
    );
    assert_eq!(manager.open_count(), 2);

    drop(handle_a);
    manager
        .acquire(wt_b, 1000)
        .await
        .expect("re-acquire B (cache hit, bumps recency, triggers eviction sweep)");

    assert!(
        !manager.is_cached(wt_a),
        "A became evictable the moment it was no longer held"
    );
    assert!(manager.is_cached(wt_b));
    assert_eq!(manager.open_count(), 1);
}

/// **"corrupt cold reopen triggers rebuild"**: a fresh manager's cache miss
/// on a corrupted shard self-heals via `open_and_validate`'s real
/// validate/rebuild path, not a raw `store.open()`.
#[tokio::test]
async fn corrupt_cold_shard_self_heals_on_acquire() {
    let (_home, layout, db) = open_state();
    let db = Arc::new(db);
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 40).await;

    // Corrupt the persisted points file directly (mirrors
    // `rebuild.rs::unopenable_shard_is_quarantined_and_rebuilt`).
    std::fs::write(shard_dir.join("points"), "0a\tnothex\n").expect("corrupt points file");

    let manager = ShardManager::new(
        db,
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(FakeVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    );

    let handle = manager
        .acquire(wt, 5000)
        .await
        .expect("cold acquire self-heals a corrupt shard");
    assert_eq!(
        handle.point_count().expect("point count"),
        2,
        "the single seeded occurrence's 2 required representations \
         (code_raw + code_context) were re-upserted by rebuild"
    );
    let head = handle
        .read_head()
        .expect("read head")
        .expect("head present after rebuild");
    assert_eq!(head.point_count, 2);
}

/// **"remove cancels background rebuild safely"**: forcing a removal while a
/// fill is stuck behind a saturated `state.sqlite` writer neither panics nor
/// hangs, and a subsequent fresh `acquire` still converges to a valid,
/// correctly-rebuilt handle.
#[tokio::test]
async fn remove_cancels_inflight_fill_and_a_fresh_acquire_self_heals() {
    let (_home, layout, db) = open_state();
    let db = Arc::new(db);
    let (wt, _gen_a, shard_dir) = established(&db, &layout, 50).await;
    std::fs::write(shard_dir.join("points"), "0a\tnothex\n").expect("corrupt points file");

    // Occupy the sole state.sqlite writer thread so the manager's background
    // fill (which needs a `mark_dirty` transaction to even begin repairing)
    // cannot progress past it until released — mirrors
    // `crates/store/tests/state.rs::queue_saturation_waits_then_cancels_cleanly`.
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (proceed_tx, proceed_rx) = std::sync::mpsc::channel::<()>();
    let writer_a = db.writer().clone();
    let job_a = tokio::spawn(async move {
        writer_a
            .transaction(move |_tx| {
                started_tx.send(()).ok();
                proceed_rx.recv().ok();
                Ok::<(), local_rag_store::rusqlite::Error>(())
            })
            .await
    });
    tokio::task::spawn_blocking(move || started_rx.recv().expect("job A started"))
        .await
        .expect("join started-wait");

    let manager = Arc::new(ShardManager::new(
        db.clone(),
        Arc::new(FakeProjectionStore::new()),
        layout,
        params(),
        Arc::new(FakeVectors),
        Arc::new(SeqUuidV7::new()),
        8,
    ));

    let mgr = manager.clone();
    let acquire_task = tokio::spawn(async move { mgr.acquire(wt, 6000).await });

    // Deterministic (no sleep): wait until the fill has registered itself,
    // then give it real scheduling opportunities to run its synchronous
    // prefix through to its own suspension behind job A.
    while !manager.is_inflight(wt) {
        tokio::task::yield_now().await;
    }
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    manager.remove(wt);

    // Release job A so anything already enqueued behind it — including a
    // possibly-orphaned `mark_dirty` from the now-aborted fill — actually
    // runs and commits (state.sqlite writes are never torn by cancellation).
    proceed_tx.send(()).ok();
    job_a.await.expect("join job A").expect("job A committed");

    match acquire_task.await.expect("join acquire task") {
        // Cancelled (the common case) or won the race and completed first —
        // both are safe; the meaningful proof is reaching this line without
        // a panic or a hang.
        Ok(_) | Err(AcquireError::Removed) => {}
        Err(other) => panic!("unexpected acquire error: {other}"),
    }

    let handle = manager
        .acquire(wt, 7000)
        .await
        .expect("fresh acquire self-heals regardless of the race outcome above");
    assert_eq!(handle.point_count().expect("point count"), 2);
}
