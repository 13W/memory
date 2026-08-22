//! T06-01 integration tests for pin-root calculation (spec 06 §5): the DB-facing
//! readers over real `generation` rows, worktree isolation of the "last `K`"
//! window, and agreement between [`pinned_generation_roots`] and the pure
//! [`mark_pins`] on the same data.
//!
//! The pure policy matrix (state roots, K/T boundaries, lease expiry, union,
//! determinism) lives in the `retention` module's unit tests; these exercise the
//! schema-backed path.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`/`created_at`
//! literals, and ids from [`uuidv7_from`] with fixed entropy (no wall clock or
//! entropy source). Writes flow through [`StateWriter::transaction`]; reads use
//! [`StateDb::open_read`].

use std::collections::BTreeSet;

use local_rag_core::config::StorageConfig;
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    GenerationState, WorktreeKind, allocate_generation, create_repository, create_worktree,
    transition_generation,
};
use local_rag_store::{
    EdgeResolution, ExternalPins, JobLease, NewContentBlob, NewFileRevision, NewOccurrence,
    NewParsedUnit, NewResolvedEdge, NewUnresolvedReference, NewlineStyle, RetentionParams,
    SkipReason, SourceCompression, StateDb, SweepReport, UnitKind, fail_superseded_generations,
    generation_meta_for_worktree, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, insert_resolved_edge,
    insert_skipped_file, insert_unresolved_reference, mark_pins, pinned_generation_roots,
    plan_sweep, run_sweep, run_sweep_with_batch,
};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (production
/// migration set: registry v1, worktree v2, code v3).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`, never touching the
/// clock or entropy source.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository and one `active` main worktree under it; returns `worktree_id`.
async fn worktree(db: &StateDb, seed: u8) -> String {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(100));
    let (repo0, wt0) = (repo.clone(), wt.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo0, None, 1000)?;
            create_worktree(tx, &wt0, &repo0, WorktreeKind::Main, 1000)
        })
        .await
        .expect("create repo + worktree");
    wt
}

/// Allocate one generation for `worktree_id` (born `building`) with an explicit
/// `created_at`; returns its id.
async fn allocate_at(db: &StateDb, worktree_id: &str, gen_seed: u8, created_at: i64) -> String {
    let genr = uuid(gen_seed);
    let (w, g) = (worktree_id.to_string(), genr.clone());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, created_at).map(|_| ()))
        .await
        .expect("allocate generation");
    genr
}

/// Drive a freshly-allocated (`building`) generation to `target` through legal
/// transitions (spec 04 §1): `building → projection_ready → active → retiring`, or
/// `building → failed`.
async fn drive_to(db: &StateDb, generation_id: &str, target: GenerationState) {
    let path: &[GenerationState] = match target {
        GenerationState::Building => &[],
        GenerationState::ProjectionReady => &[GenerationState::ProjectionReady],
        GenerationState::Active => &[GenerationState::ProjectionReady, GenerationState::Active],
        GenerationState::Retiring => &[
            GenerationState::ProjectionReady,
            GenerationState::Active,
            GenerationState::Retiring,
        ],
        GenerationState::Failed => &[GenerationState::Failed],
    };
    for &to in path {
        let g = generation_id.to_string();
        db.writer()
            .transaction(move |tx| transition_generation(tx, &g, to))
            .await
            .expect("transition tx (infrastructure)")
            .expect("legal transition");
    }
}

// ---------------------------------------------------------------------------
// D-088: a generation a newer one left behind must stop being a pin root.
// `mark_pins` pins every `building`/`projection_ready` generation
// unconditionally, and nothing ever retries an abandoned one — so without this
// they accumulate forever, and the embedding backfill walks every one of them on
// every cycle. Measured on the owner's store: 3086 of them.
// ---------------------------------------------------------------------------

/// Retire everything superseded by `keep`, looping the way a caller must:
/// until a call moves nothing. Looping on "was the batch full?" instead spins
/// forever against an implementation that selects rows but moves none.
async fn fail_superseded(db: &StateDb, worktree_id: &str, keep: &str, limit: usize) -> usize {
    let mut total = 0;
    loop {
        let (w, k) = (worktree_id.to_string(), keep.to_string());
        let n = db
            .writer()
            .transaction(move |tx| fail_superseded_generations(tx, &w, &k, limit))
            .await
            .expect("fail superseded tx");
        total += n;
        if n == 0 {
            return total;
        }
    }
}

async fn state_of(db: &StateDb, worktree_id: &str, generation_id: &str) -> GenerationState {
    generation_meta_for_worktree(&db.open_read().expect("read conn"), worktree_id)
        .expect("meta")
        .into_iter()
        .find(|g| g.generation_id == generation_id)
        .expect("generation exists")
        .state
}

#[tokio::test]
async fn a_superseded_projection_ready_generation_stops_being_a_pin_root() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 40).await;

    let older = allocate_at(&db, &wt, 41, 1000).await;
    drive_to(&db, &older, GenerationState::ProjectionReady).await;
    let newer = allocate_at(&db, &wt, 42, 2000).await;
    drive_to(&db, &newer, GenerationState::ProjectionReady).await;

    // Before: both are pin roots, which is the ratchet itself.
    let params = RetentionParams {
        keep_last_k: 2,
        window_ms: 7 * 24 * 60 * 60 * 1000,
    };
    let before = pinned_generation_roots(
        &db.open_read().expect("read conn"),
        &wt,
        &params,
        &ExternalPins::default(),
        3000,
    )
    .expect("pins")
    .generations;
    assert!(before.contains(&older) && before.contains(&newer));

    assert_eq!(fail_superseded(&db, &wt, &newer, 500).await, 1);

    assert_eq!(state_of(&db, &wt, &older).await, GenerationState::Failed);
    assert_eq!(
        state_of(&db, &wt, &newer).await,
        GenerationState::ProjectionReady,
        "the generation the cycle is about to activate must be untouched"
    );

    let after = pinned_generation_roots(
        &db.open_read().expect("read conn"),
        &wt,
        &params,
        &ExternalPins::default(),
        3000,
    )
    .expect("pins")
    .generations;
    assert!(
        !after.contains(&older),
        "a retired generation must leave the pin set — otherwise the backfill still walks it"
    );
    assert!(after.contains(&newer));
}

#[tokio::test]
async fn nothing_newer_than_the_kept_generation_is_touched() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 50).await;

    let older = allocate_at(&db, &wt, 51, 1000).await;
    drive_to(&db, &older, GenerationState::ProjectionReady).await;
    let keep = allocate_at(&db, &wt, 52, 2000).await;
    drive_to(&db, &keep, GenerationState::ProjectionReady).await;
    // A concurrent build that started after the one we are activating.
    let newer = allocate_at(&db, &wt, 53, 3000).await;

    assert_eq!(fail_superseded(&db, &wt, &keep, 500).await, 1);

    assert_eq!(state_of(&db, &wt, &older).await, GenerationState::Failed);
    assert_eq!(
        state_of(&db, &wt, &newer).await,
        GenerationState::Building,
        "a build newer than the kept generation is still live work"
    );
}

#[tokio::test]
async fn an_unknown_keep_generation_retires_nothing() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 60).await;
    let ready = allocate_at(&db, &wt, 61, 1000).await;
    drive_to(&db, &ready, GenerationState::ProjectionReady).await;

    // "Older than a generation that does not exist" must not read as "older
    // than everything" — that would be a silent purge.
    assert_eq!(fail_superseded(&db, &wt, &uuid(200), 500).await, 0);
    assert_eq!(
        state_of(&db, &wt, &ready).await,
        GenerationState::ProjectionReady
    );
}

#[tokio::test]
async fn a_full_batch_reports_itself_so_the_caller_comes_back() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 70).await;

    let mut older = Vec::new();
    for seed in 71..=73u8 {
        let g = allocate_at(&db, &wt, seed, 1000 + i64::from(seed)).await;
        drive_to(&db, &g, GenerationState::ProjectionReady).await;
        older.push(g);
    }
    let keep = allocate_at(&db, &wt, 80, 5000).await;
    drive_to(&db, &keep, GenerationState::ProjectionReady).await;

    // Batch of one: the loop must run three times, not stop after the first.
    assert_eq!(fail_superseded(&db, &wt, &keep, 1).await, 3);
    for g in &older {
        assert_eq!(state_of(&db, &wt, g).await, GenerationState::Failed);
    }
}

/// `pinned_generation_roots` over real rows must equal the pure `mark_pins` on the
/// same loaded metadata, and it must include the unconditional state roots.
#[tokio::test]
async fn db_roots_match_pure_mark_and_cover_state_roots() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    // building, projection_ready, active, retiring, failed — one of each.
    let g_build = allocate_at(&db, &wt, 10, 1000).await;
    let g_ready = allocate_at(&db, &wt, 11, 1000).await;
    drive_to(&db, &g_ready, GenerationState::ProjectionReady).await;
    let g_active = allocate_at(&db, &wt, 12, 1000).await;
    drive_to(&db, &g_active, GenerationState::Active).await;
    let g_retiring = allocate_at(&db, &wt, 13, 1000).await;
    drive_to(&db, &g_retiring, GenerationState::Retiring).await;
    let g_failed = allocate_at(&db, &wt, 14, 1000).await;
    drive_to(&db, &g_failed, GenerationState::Failed).await;

    let params = RetentionParams {
        keep_last_k: 0,
        window_ms: 0,
    };
    let ext = ExternalPins::default();
    let now = 2000;

    let read = db.open_read().expect("read conn");
    let roots = pinned_generation_roots(&read, &wt, &params, &ext, now).expect("roots");

    // building + projection_ready + active are pinned; retiring (K=0/window=0) and
    // failed are not.
    let expected: BTreeSet<String> =
        BTreeSet::from([g_build.clone(), g_ready.clone(), g_active.clone()]);
    assert_eq!(roots.generations, expected);
    assert!(!roots.generations.contains(&g_retiring));
    assert!(!roots.generations.contains(&g_failed));

    // The DB path equals the pure path on the same loaded rows.
    let meta = generation_meta_for_worktree(&read, &wt).expect("meta");
    assert_eq!(roots, mark_pins(&meta, &params, &ext, now));
}

/// The "last `K`" window is per worktree: each worktree keeps its own K retiring
/// generations, and one worktree's pins never include another's.
#[tokio::test]
async fn last_k_is_isolated_per_worktree() {
    let (_home, db) = open_state();
    let wt_a = worktree(&db, 1).await;
    let wt_b = worktree(&db, 2).await;

    // Three retiring generations in each worktree (numbers 1,2,3 per worktree).
    let mut a_ids = Vec::new();
    for seed in [20, 21, 22] {
        let g = allocate_at(&db, &wt_a, seed, 1000).await;
        drive_to(&db, &g, GenerationState::Retiring).await;
        a_ids.push(g);
    }
    let mut b_ids = Vec::new();
    for seed in [30, 31, 32] {
        let g = allocate_at(&db, &wt_b, seed, 1000).await;
        drive_to(&db, &g, GenerationState::Retiring).await;
        b_ids.push(g);
    }

    let params = RetentionParams {
        keep_last_k: 2,
        window_ms: 0,
    };
    let ext = ExternalPins::default();
    let now = 2000;
    let read = db.open_read().expect("read conn");

    let roots_a = pinned_generation_roots(&read, &wt_a, &params, &ext, now).expect("roots a");
    // Worktree A keeps its own last two (numbers 2 and 3 → seeds 21, 22).
    assert_eq!(
        roots_a.generations,
        BTreeSet::from([a_ids[1].clone(), a_ids[2].clone()])
    );
    // None of worktree B's generations leak into A's pins.
    for b in &b_ids {
        assert!(!roots_a.generations.contains(b), "B gen {b} leaked into A");
    }

    let roots_b = pinned_generation_roots(&read, &wt_b, &params, &ext, now).expect("roots b");
    assert_eq!(
        roots_b.generations,
        BTreeSet::from([b_ids[1].clone(), b_ids[2].clone()])
    );
}

/// The window `T` is applied against `created_at` over real rows: with K=0, only
/// retiring generations created within the window are pinned.
#[tokio::test]
async fn window_over_created_at_from_real_rows() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    // Two retiring generations, one born old, one born recent.
    let g_old = allocate_at(&db, &wt, 40, 1_000).await;
    drive_to(&db, &g_old, GenerationState::Retiring).await;
    let g_recent = allocate_at(&db, &wt, 41, 9_500).await;
    drive_to(&db, &g_recent, GenerationState::Retiring).await;

    let params = RetentionParams {
        keep_last_k: 0,
        window_ms: 1_000, // now=10_000 → floor=9_000
    };
    let now = 10_000;
    let read = db.open_read().expect("read conn");
    let roots =
        pinned_generation_roots(&read, &wt, &params, &ExternalPins::default(), now).expect("roots");

    assert_eq!(roots.generations, BTreeSet::from([g_recent]));
    assert!(!roots.generations.contains(&g_old));
}

/// A non-expired lease pins an otherwise-unretained retiring generation over the
/// DB path; the default (empty) `ExternalPins` leaves it unpinned.
#[tokio::test]
async fn lease_pins_over_db_path() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    let g = allocate_at(&db, &wt, 50, 1_000).await;
    drive_to(&db, &g, GenerationState::Retiring).await;

    let params = RetentionParams {
        keep_last_k: 0,
        window_ms: 0,
    };
    let now = 5_000;
    let read = db.open_read().expect("read conn");

    // Empty external pins: nothing keeps the retiring generation.
    let bare =
        pinned_generation_roots(&read, &wt, &params, &ExternalPins::default(), now).expect("bare");
    assert!(bare.generations.is_empty());

    // A live lease pins it.
    let leased = ExternalPins {
        leases: vec![JobLease {
            generation_id: g.clone(),
            lease_until_ms: now + 1,
        }],
        ..ExternalPins::default()
    };
    let roots = pinned_generation_roots(&read, &wt, &params, &leased, now).expect("leased");
    assert_eq!(roots.generations, BTreeSet::from([g]));
}

/// A worktree with no generations pins nothing; `from_storage_config` wiring works
/// end-to-end (defaults keep nothing when there are no retiring rows).
#[tokio::test]
async fn empty_worktree_pins_nothing() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    let params = RetentionParams::from_storage_config(&StorageConfig::default());
    let read = db.open_read().expect("read conn");
    let roots =
        pinned_generation_roots(&read, &wt, &params, &ExternalPins::default(), 10_000).expect("r");
    assert!(roots.generations.is_empty());
    assert!(roots.referenced_file_revisions.is_empty());
}

// ---------------------------------------------------------------------------
// T06-02 sweep tests. The content graph (revision/blob/unit/occurrence/edge) is
// built directly through the `code`-side inserts (the group-05 generation builder
// lives in the `index` crate and is not available here), mirroring how the group-03
// code tests assemble fixtures. Ids for the content side are plain readable strings
// (the columns are TEXT with no format check); generation ids come from the registry
// helpers above.
// ---------------------------------------------------------------------------

/// Read a single `COUNT(*)` (or any scalar `i64`) over a fresh read connection.
fn scalar(db: &StateDb, sql: &str) -> i64 {
    let conn = db.open_read().expect("read conn");
    conn.query_row(sql, [], |r| r.get(0)).expect("scalar query")
}

/// Rows in `table` (whole table).
fn rows(db: &StateDb, table: &str) -> i64 {
    scalar(db, &format!("SELECT COUNT(*) FROM {table}"))
}

/// Insert a `content_blob` identity row.
async fn blob(db: &StateDb, blob_id: &str) {
    let blob_id = blob_id.to_string();
    db.writer()
        .transaction(move |tx| {
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &blob_id,
                    language: "rust",
                    algo_version: 1,
                    normalization_version: 1,
                },
                1000,
            )
        })
        .await
        .expect("insert content_blob");
}

/// Insert a `file_revision`; `content_hash` must be distinct per revision (the
/// `UNIQUE (content_hash, parser_fingerprint)` reuse key).
async fn revision(db: &StateDb, rev_id: &str, content_hash: &str) {
    let (rev_id, content_hash) = (rev_id.to_string(), content_hash.to_string());
    db.writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &rev_id,
                    content_hash: &content_hash,
                    parser_fingerprint: "fp",
                    source_blob: b"x",
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: 1,
                },
                1000,
            )
        })
        .await
        .expect("insert file_revision");
}

/// Insert a `parsed_unit` of `rev_id` (self-referencing `parent` for nested units).
/// `unit_id` doubles as the `syntax_locator` so the natural key stays unique.
async fn unit(db: &StateDb, unit_id: &str, rev_id: &str, blob_id: &str, parent: Option<&str>) {
    let (unit_id, rev_id, blob_id, parent) = (
        unit_id.to_string(),
        rev_id.to_string(),
        blob_id.to_string(),
        parent.map(str::to_string),
    );
    db.writer()
        .transaction(move |tx| {
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &unit_id,
                    file_revision_id: &rev_id,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: &unit_id,
                    blob_id: &blob_id,
                    span_start: 0,
                    span_end: 1,
                    local_name: None,
                    kind: None,
                    parent_unit_id: parent.as_deref(),
                },
            )
        })
        .await
        .expect("insert parsed_unit");
}

/// Bind `path` in `generation_id` to `rev_id` (a `generation_file` member row).
async fn gen_file(db: &StateDb, generation_id: &str, path: &str, rev_id: &str) {
    let (g, p, r) = (
        generation_id.to_string(),
        path.to_string(),
        rev_id.to_string(),
    );
    db.writer()
        .transaction(move |tx| insert_generation_file(tx, &g, &p, &p, &r))
        .await
        .expect("insert generation_file");
}

/// Insert one occurrence of `unit_id` at `path` in `generation_id`.
async fn occurrence(db: &StateDb, occ_id: &str, generation_id: &str, path: &str, unit_id: &str) {
    let (occ, g, p, u) = (
        occ_id.to_string(),
        generation_id.to_string(),
        path.to_string(),
        unit_id.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ,
                    generation_id: &g,
                    normalized_path: &p,
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect("insert occurrence");
}

/// Insert a `resolved_graph_edge` between two occurrences of `generation_id`.
async fn edge(db: &StateDb, generation_id: &str, src: &str, dst: &str) {
    let (g, s, d) = (generation_id.to_string(), src.to_string(), dst.to_string());
    db.writer()
        .transaction(move |tx| {
            insert_resolved_edge(
                tx,
                &NewResolvedEdge {
                    generation_id: &g,
                    src_occurrence_id: &s,
                    dst_occurrence_id: &d,
                    edge_kind: "import",
                    resolution: EdgeResolution::Heuristic,
                },
            )
        })
        .await
        .expect("insert resolved_graph_edge");
}

/// Insert a `skipped_file` row in `generation_id`.
async fn skipped(db: &StateDb, generation_id: &str, path: &str) {
    let (g, p) = (generation_id.to_string(), path.to_string());
    db.writer()
        .transaction(move |tx| insert_skipped_file(tx, &g, &p, SkipReason::Binary, None))
        .await
        .expect("insert skipped_file");
}

/// Insert an `unresolved_reference` from `unit_id` in `rev_id`.
async fn unresolved(db: &StateDb, rev_id: &str, unit_id: &str) {
    let (r, u) = (rev_id.to_string(), unit_id.to_string());
    db.writer()
        .transaction(move |tx| {
            insert_unresolved_reference(
                tx,
                &NewUnresolvedReference {
                    file_revision_id: &r,
                    source_unit_id: &u,
                    reference_text: "dep",
                    reference_kind: "import",
                },
            )
        })
        .await
        .expect("insert unresolved_reference");
}

/// K=0 / T=0: pins nothing by the retention window, so every `retiring`/`failed`
/// generation becomes a sweep candidate.
fn sweep_everything_retired() -> RetentionParams {
    RetentionParams {
        keep_last_k: 0,
        window_ms: 0,
    }
}

/// A pinned (`active`) generation's rows — and the content it references — survive a
/// sweep, while an unpinned `retiring` generation and its now-orphaned content go.
#[tokio::test]
async fn pinned_rows_survive() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    // Pinned: an active generation with a full content graph.
    let g_keep = allocate_at(&db, &wt, 10, 1000).await;
    drive_to(&db, &g_keep, GenerationState::Active).await;
    blob(&db, "blob-keep").await;
    revision(&db, "rev-keep", "hash-keep").await;
    unit(&db, "unit-keep", "rev-keep", "blob-keep", None).await;
    gen_file(&db, &g_keep, "keep.rs", "rev-keep").await;
    occurrence(&db, "occ-keep", &g_keep, "keep.rs", "unit-keep").await;
    edge(&db, &g_keep, "occ-keep", "occ-keep").await;

    // Candidate: a retiring generation with its own (distinct) content graph.
    let g_drop = allocate_at(&db, &wt, 11, 1000).await;
    drive_to(&db, &g_drop, GenerationState::Retiring).await;
    blob(&db, "blob-drop").await;
    revision(&db, "rev-drop", "hash-drop").await;
    unit(&db, "unit-drop", "rev-drop", "blob-drop", None).await;
    gen_file(&db, &g_drop, "drop.rs", "rev-drop").await;
    occurrence(&db, "occ-drop", &g_drop, "drop.rs", "unit-drop").await;

    let report = run_sweep(
        &db,
        &sweep_everything_retired(),
        &ExternalPins::default(),
        2000,
    )
    .await
    .expect("sweep");

    // Only the retiring generation's rows were removed.
    assert_eq!(report.generations, 1, "one generation swept");
    assert_eq!(report.file_revisions, 1);
    assert_eq!(report.content_blobs, 1);

    // The pinned generation and everything it references is intact.
    assert_eq!(
        scalar(
            &db,
            &format!("SELECT COUNT(*) FROM generation WHERE generation_id = '{g_keep}'")
        ),
        1
    );
    assert_eq!(
        rows(&db, "generation_file"),
        1,
        "only keep's membership remains"
    );
    assert_eq!(rows(&db, "generation_unit_occurrence"), 1);
    assert_eq!(rows(&db, "resolved_graph_edge"), 1);
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM file_revision WHERE file_revision_id = 'rev-keep'"
        ),
        1
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM parsed_unit WHERE unit_id = 'unit-keep'"
        ),
        1
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM content_blob WHERE blob_id = 'blob-keep'"
        ),
        1
    );

    // The candidate's rows are all gone.
    assert_eq!(
        scalar(
            &db,
            &format!("SELECT COUNT(*) FROM generation WHERE generation_id = '{g_drop}'")
        ),
        0
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM file_revision WHERE file_revision_id = 'rev-drop'"
        ),
        0
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM content_blob WHERE blob_id = 'blob-drop'"
        ),
        0
    );
}

/// A wholly-unpinned generation with a complete graph (membership, skips,
/// occurrences, edges, unresolved refs, nested units, revision, blob) is swept down
/// to zero rows in every table.
#[tokio::test]
async fn orphan_graph_fully_removed() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    let g = allocate_at(&db, &wt, 10, 1000).await;
    drive_to(&db, &g, GenerationState::Retiring).await;

    blob(&db, "blob").await;
    revision(&db, "rev", "hash").await;
    // A nested unit tree: root → child.
    unit(&db, "unit-root", "rev", "blob", None).await;
    unit(&db, "unit-child", "rev", "blob", Some("unit-root")).await;
    unresolved(&db, "rev", "unit-root").await;
    gen_file(&db, &g, "f.rs", "rev").await;
    skipped(&db, &g, "vendor.bin").await;
    occurrence(&db, "occ-root", &g, "f.rs", "unit-root").await;
    occurrence(&db, "occ-child", &g, "f.rs", "unit-child").await;
    edge(&db, &g, "occ-root", "occ-child").await;

    let report = run_sweep(
        &db,
        &sweep_everything_retired(),
        &ExternalPins::default(),
        2000,
    )
    .await
    .expect("sweep");

    assert_eq!(report.edges, 1);
    assert_eq!(report.occurrences, 2);
    assert_eq!(report.generation_files, 1);
    assert_eq!(report.skipped_files, 1);
    assert_eq!(report.generations, 1);
    assert_eq!(report.unresolved_references, 1);
    assert_eq!(report.parsed_units, 2);
    assert_eq!(report.file_revisions, 1);
    assert_eq!(report.content_blobs, 1);

    for table in [
        "resolved_graph_edge",
        "generation_unit_occurrence",
        "generation_file",
        "skipped_file",
        "generation",
        "unresolved_reference",
        "parsed_unit",
        "file_revision",
        "content_blob",
    ] {
        assert_eq!(rows(&db, table), 0, "{table} fully swept");
    }
}

/// A content-shared `file_revision` survives while any pinned generation references
/// it, and is swept only once its last referencing generation is gone.
#[tokio::test]
async fn shared_revision_retained_until_final_ref() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    // One revision shared by two generations at two paths (structural sharing).
    blob(&db, "blob-s").await;
    revision(&db, "rev-s", "hash-s").await;
    unit(&db, "unit-s", "rev-s", "blob-s", None).await;

    let g_keep = allocate_at(&db, &wt, 10, 1000).await;
    drive_to(&db, &g_keep, GenerationState::Active).await;
    gen_file(&db, &g_keep, "a.rs", "rev-s").await;
    occurrence(&db, "occ-keep", &g_keep, "a.rs", "unit-s").await;

    let g_drop = allocate_at(&db, &wt, 11, 1000).await;
    drive_to(&db, &g_drop, GenerationState::Retiring).await;
    gen_file(&db, &g_drop, "b.rs", "rev-s").await;
    occurrence(&db, "occ-drop", &g_drop, "b.rs", "unit-s").await;

    // First sweep: the retiring generation goes, but the shared revision stays
    // because the active generation still references it.
    let r1 = run_sweep(
        &db,
        &sweep_everything_retired(),
        &ExternalPins::default(),
        2000,
    )
    .await
    .expect("sweep 1");
    assert_eq!(r1.generations, 1);
    assert_eq!(r1.file_revisions, 0, "shared revision retained");
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM file_revision WHERE file_revision_id = 'rev-s'"
        ),
        1
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM parsed_unit WHERE unit_id = 'unit-s'"
        ),
        1
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM content_blob WHERE blob_id = 'blob-s'"
        ),
        1
    );

    // Retire the last holder and sweep again: now the revision is orphaned.
    db.writer()
        .transaction({
            let g = g_keep.clone();
            move |tx| transition_generation(tx, &g, GenerationState::Retiring)
        })
        .await
        .expect("transition tx")
        .expect("active → retiring");

    let r2 = run_sweep(
        &db,
        &sweep_everything_retired(),
        &ExternalPins::default(),
        2000,
    )
    .await
    .expect("sweep 2");
    assert_eq!(r2.generations, 1);
    assert_eq!(r2.file_revisions, 1, "revision swept after final ref");
    assert_eq!(r2.parsed_units, 1);
    assert_eq!(r2.content_blobs, 1);
    assert_eq!(rows(&db, "file_revision"), 0);
    assert_eq!(rows(&db, "parsed_unit"), 0);
    assert_eq!(rows(&db, "content_blob"), 0);
}

/// A retiring generation pinned by an external `file_revision` reference keeps that
/// revision even though the generation itself is swept.
#[tokio::test]
async fn external_revision_pin_retains_shared_content() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    blob(&db, "blob-e").await;
    revision(&db, "rev-e", "hash-e").await;
    unit(&db, "unit-e", "rev-e", "blob-e", None).await;

    let g = allocate_at(&db, &wt, 10, 1000).await;
    drive_to(&db, &g, GenerationState::Retiring).await;
    gen_file(&db, &g, "e.rs", "rev-e").await;
    occurrence(&db, "occ-e", &g, "e.rs", "unit-e").await;

    let external = ExternalPins {
        referenced_file_revisions: BTreeSet::from(["rev-e".to_string()]),
        ..ExternalPins::default()
    };
    let report = run_sweep(&db, &sweep_everything_retired(), &external, 2000)
        .await
        .expect("sweep");

    // The generation and its membership/occurrence are swept, but the externally
    // pinned revision (and its unit/blob) survive.
    assert_eq!(report.generations, 1);
    assert_eq!(report.file_revisions, 0);
    assert_eq!(report.parsed_units, 0);
    assert_eq!(report.content_blobs, 0);
    assert_eq!(rows(&db, "file_revision"), 1);
    assert_eq!(rows(&db, "parsed_unit"), 1);
}

/// A dry run reports the generations and rows it would delete and mutates no
/// canonical row; a subsequent real sweep deletes exactly the reported counts.
#[tokio::test]
async fn dry_run_mutates_nothing() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    let g = allocate_at(&db, &wt, 10, 1000).await;
    drive_to(&db, &g, GenerationState::Retiring).await;
    blob(&db, "blob").await;
    revision(&db, "rev", "hash").await;
    unit(&db, "unit", "rev", "blob", None).await;
    gen_file(&db, &g, "f.rs", "rev").await;
    occurrence(&db, "occ", &g, "f.rs", "unit").await;

    let tables = [
        "generation",
        "generation_file",
        "generation_unit_occurrence",
        "file_revision",
        "parsed_unit",
        "content_blob",
    ];
    let before: Vec<i64> = tables.iter().map(|t| rows(&db, t)).collect();

    let plan = plan_sweep(
        &db,
        &sweep_everything_retired(),
        &ExternalPins::default(),
        2000,
    )
    .await
    .expect("plan");

    assert_eq!(plan.candidate_generations, vec![g.clone()]);
    assert!(!plan.would_delete.is_empty(), "dry run projects deletions");
    assert_eq!(plan.would_delete.generations, 1);
    assert_eq!(plan.would_delete.file_revisions, 1);

    // Nothing changed.
    let after: Vec<i64> = tables.iter().map(|t| rows(&db, t)).collect();
    assert_eq!(before, after, "dry run must not mutate canonical tables");

    // The real sweep deletes exactly what the plan projected.
    let report = run_sweep(
        &db,
        &sweep_everything_retired(),
        &ExternalPins::default(),
        2000,
    )
    .await
    .expect("sweep");
    assert_eq!(report, plan.would_delete, "plan matches actual deletions");
}

/// A deep `parsed_unit` tree whose deletion spans several batches is removed
/// leaf-first, so the self-referential `parent_unit_id` foreign key never dangles at
/// a statement boundary even with a batch ceiling of one row.
#[tokio::test]
async fn deep_unit_tree_deleted_leaf_first_across_batches() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;

    let g = allocate_at(&db, &wt, 10, 1000).await;
    drive_to(&db, &g, GenerationState::Retiring).await;
    blob(&db, "blob").await;
    revision(&db, "rev", "hash").await;
    // A five-deep chain: u1 (root) ← u2 ← u3 ← u4 ← u5.
    unit(&db, "u1", "rev", "blob", None).await;
    unit(&db, "u2", "rev", "blob", Some("u1")).await;
    unit(&db, "u3", "rev", "blob", Some("u2")).await;
    unit(&db, "u4", "rev", "blob", Some("u3")).await;
    unit(&db, "u5", "rev", "blob", Some("u4")).await;
    gen_file(&db, &g, "f.rs", "rev").await;

    // Batch ceiling of 1 forces one leaf per statement — the strictest leaf-first case.
    let report = run_sweep_with_batch(
        &db,
        &sweep_everything_retired(),
        &ExternalPins::default(),
        2000,
        1,
    )
    .await
    .expect("sweep");

    assert_eq!(report.parsed_units, 5);
    assert_eq!(report.file_revisions, 1);
    assert_eq!(
        rows(&db, "parsed_unit"),
        0,
        "whole tree removed, no FK error"
    );
    assert_eq!(rows(&db, "file_revision"), 0);
    assert_eq!(rows(&db, "content_blob"), 0);
}

/// A sweep with nothing to collect is a no-op that reports zero and leaves an
/// all-`active` store untouched (idempotence of the empty case).
#[tokio::test]
async fn empty_sweep_is_a_noop() {
    let (_home, db) = open_state();
    let wt = worktree(&db, 1).await;
    let g = allocate_at(&db, &wt, 10, 1000).await;
    drive_to(&db, &g, GenerationState::Active).await;
    revision(&db, "rev", "hash").await;
    gen_file(&db, &g, "a.rs", "rev").await;

    let report: SweepReport = run_sweep(
        &db,
        &sweep_everything_retired(),
        &ExternalPins::default(),
        2000,
    )
    .await
    .expect("sweep");
    assert!(report.is_empty());
    assert_eq!(rows(&db, "generation"), 1);
    assert_eq!(rows(&db, "generation_file"), 1);
}

/// `D-094`: the sweep's scaffolding and its planning pass go through the
/// read-only entry point; only the batched delete takes the write lock.
///
/// Structural, in the shape `D-054` established, because the alternative is
/// invisible: routing a read-only pass back through `transaction()` compiles,
/// passes every behavioural test, and only shows up as other processes failing
/// to write — which is exactly how `D-094` reached a live store. Counting, not
/// pattern-matching, so a renamed helper reads as a mismatch rather than as
/// silence.
#[test]
fn only_the_batched_delete_takes_the_write_lock() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/retention.rs"))
        .expect("read retention.rs");

    let code: Vec<&str> = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();

    let read_only = code
        .iter()
        .filter(|line| line.contains(".read_transaction("))
        .count();
    let writing = code
        .iter()
        .filter(|line| line.contains(".transaction(") && !line.contains(".read_transaction("))
        .count();

    assert_eq!(
        (read_only, writing),
        (3, 1),
        "expected `plan_sweep`, `setup_scratch` and `drop_scratch` on the read-only path and only \
         the batched delete on the writing one (D-094)"
    );
}
