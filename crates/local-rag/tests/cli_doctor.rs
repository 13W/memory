//! `local-rag doctor [--worktree <id>] [--json]` acceptance tests (spec 11
//! §6, T16-03), driving the real compiled binary — mirrors `tests/cli_gc.rs`'s
//! own `open_layout`/`run_cli` helpers (duplicated here per this crate's
//! established per-file-fixture convention) and its philosophy: this file's
//! job is to prove the *wiring* (each section's fault is surfaced, nothing is
//! ever mutated, `--worktree` narrows correctly, recovery via the
//! already-tested `rebuild` command makes doctor clean again), not to
//! re-prove `check_fts`/`check_dense`/`diagnose_versions`/`audit_permissions`'s
//! own exhaustive divergence tables — those live where each function does
//! (`crates/store/tests/{migrate,cache_diagnosis,fts_validate}.rs`,
//! `crates/projection/tests/rebuild.rs`).
//!
//! Every non-`--dense`-recovery scenario here is deliberately ONNX-free:
//! `--fts` re-derives the FTS view from already-indexed content (no embedder
//! at all, `cli::rebuild`'s own module doc), and a genuine on-disk dense shard
//! can be built directly via `local_rag_projection::switch` with the real
//! `BruteForceProjectionStore` backend fed synthetic vectors (a `VectorSource`
//! is an injected seam, not an ONNX Runtime call) — exactly
//! `crates/projection/tests/rebuild.rs`'s own `established()` recipe, adapted
//! to `DEFAULT_MODEL_SPACE_ID` since `doctor` hardcodes that space. Only the
//! real end-to-end "index with the real model, then `rebuild --dense`" round
//! trip needs a live embedder — that is `with_real_model` below, env-gated
//! exactly like `tests/cli_rebuild.rs`'s own module of the same name.

#![cfg(unix)]

use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{
    BruteForceProjectionStore, RepresentationKind as ProjRepresentationKind, ShardParams,
    VectorSource, switch,
};
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, DEFAULT_MODEL_SPACE_ID, DistanceMetric, FailureKind,
    GLOBAL_SCOPE_OWNER_ID, GenerationState, LEASE_DURATION_MS, MAX_NORMALIZATION_ATTEMPTS,
    MemoryKind, NewConsolidationRun, NewContentBlob, NewFileRevision, NewMemoryEntry,
    NewOccurrence, NewParsedUnit, NewlineStyle, NormalizationStatus, NormalizationWrite,
    RepresentationKey, RepresentationKind, RunState, ScopeKind, SourceCompression, StateDb,
    UnitKind, UpsertOutcome, WorktreeKind, allocate_generation, create_consolidation_run,
    create_memory_entry, create_repository, create_worktree, derive_content_blob,
    ensure_store_instance_uuid, insert_content_blob, insert_file_revision, insert_generation_file,
    insert_occurrence, insert_parsed_unit, insert_projection_state, materialize_fts, occurrence_id,
    record_run_failure, register_representation, retry_run, set_model_space_representation,
    transition_generation, transition_run, upsert_normalization,
};
use local_rag_test_support::TempHome;

const DIMS: usize = 3;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn run_cli(home: &TempHome, args: &[&str]) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand)
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
        uuidv7_from(2_000_000 + n, [0x77; 10])
    }
}

/// A fixed `DIMS`-wide vector for any occurrence — a plain injected seam, not
/// an ONNX call (see this file's module doc).
struct FakeVectors;
impl VectorSource for FakeVectors {
    fn vector(&self, _occurrence_id: &str, _kind: ProjRepresentationKind) -> Option<Vec<f32>> {
        Some(vec![1.0, 0.0, 0.0])
    }
}

/// Store-wide bootstrap every "healthy" fixture in this file needs, mirroring
/// what a real `local-rag init` does: `code_raw` registered against
/// `DEFAULT_MODEL_SPACE_ID` (without it, `params_for_model_space` — and so
/// every dense head check — refuses regardless of any worktree fixture) and
/// `store_instance_uuid`/`cache.sqlite` bound (without it, `cache_binding`
/// errors and so every FTS head check is `Unavailable`, `[BOTH LEGS
/// UNAVAILABLE]` even on a worktree that never touched either leg).
/// Idempotent — safe to call once per fixture worktree.
async fn bootstrap_store(db: &StateDb, layout: &StoreLayout) {
    db.writer()
        .transaction(move |tx| {
            let id = register_representation(
                tx,
                "cli-doctor-code-raw",
                &RepresentationKey {
                    kind: RepresentationKind::CodeRaw,
                    representation_version: 1,
                    normalization_version: 1,
                    model_id: "cli-doctor-test-model".to_string(),
                    dimensions: DIMS as u32,
                    distance_metric: DistanceMetric::Dot,
                },
                1000,
            )?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                RepresentationKind::CodeRaw,
                &id,
                true,
                1000,
            )
        })
        .await
        .expect("register default code_raw representation");

    let store_uuid = db
        .writer()
        .transaction(|tx| ensure_store_instance_uuid(tx, "cli-doctor-instance"))
        .await
        .expect("ensure store_instance_uuid");
    local_rag_store::CacheDb::open(layout.cache_db(), &store_uuid)
        .expect("open+bind cache.sqlite")
        .close();
}

/// A worktree registered but never indexed (no active tuple at all) — both
/// legs report their respective "nothing to check yet" outcome, which
/// `is_clean` treats as benign. Runs [`bootstrap_store`] first (idempotent):
/// without it, a "bare, never-indexed" fixture would misleadingly report
/// unclean on both legs regardless of this worktree's own state.
async fn seed_bare_worktree(db: &StateDb, layout: &StoreLayout, seed: u8) -> Uuid {
    bootstrap_store(db, layout).await;
    let repo = uuid(seed).to_string();
    let wt = uuid(seed.wrapping_add(1));
    let (r, w) = (repo, wt.to_string());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)
        })
        .await
        .expect("create repo + worktree");
    let w2 = wt.to_string();
    db.writer()
        .transaction(move |tx| insert_projection_state(tx, &w2, 1000))
        .await
        .expect("init projection state");
    wt
}

/// One occurrence, real enough for `materialize_fts`/`switch` to operate on
/// (mirrors `crates/projection/tests/rebuild.rs::seed_occurrence`).
async fn seed_occurrence(db: &StateDb, generation_id: &Uuid, seed: u8, path: &str) -> String {
    let gen_str = generation_id.to_string();
    let revision = uuid(seed).to_string();
    let unit = uuid(seed.wrapping_add(40)).to_string();
    let occ = occurrence_id(&gen_str, path, &unit);
    let source_text = "fn hello() {}\n";
    let derived = derive_content_blob("rust", source_text);
    let (rev, b, u, g, p, occ2) = (
        revision,
        derived.blob_id.clone(),
        unit,
        gen_str,
        path.to_string(),
        occ.clone(),
    );
    let source_size = source_text.len() as i64;
    db.writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &rev,
                    content_hash: &rev,
                    parser_fingerprint: "fp",
                    source_blob: source_text.as_bytes(),
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size,
                },
                1000,
            )?;
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &b,
                    language: "rust",
                    algo_version: derived.algo_version,
                    normalization_version: derived.normalization_version,
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
                    span_end: source_size,
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

/// A fully established, real, on-disk-valid worktree: registered `code_raw`,
/// one `projection_ready` generation with one occurrence, a real dense shard
/// via `switch()` + the real `BruteForceProjectionStore` (synthetic vectors,
/// no ONNX), and real FTS rows via `materialize_fts` (no embedder involved at
/// all — FTS is lexical). `doctor` against this worktree alone is clean.
async fn seed_indexed_worktree(state: &StateDb, layout: &StoreLayout, seed: u8) -> (Uuid, String) {
    let wt = seed_bare_worktree(state, layout, seed).await;
    let genr = uuid(seed.wrapping_add(2));
    let (w, g) = (wt.to_string(), genr.to_string());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 1000).map(|_| ()))
        .await
        .expect("allocate generation");
    let g2 = genr.to_string();
    state
        .writer()
        .transaction(move |tx| transition_generation(tx, &g2, GenerationState::ProjectionReady))
        .await
        .expect("transition tx")
        .expect("building -> projection_ready is legal");
    seed_occurrence(state, &genr, seed.wrapping_add(3), "a.rs").await;

    let ms = default_model_space();
    let shard_dir = layout.projection_shard_space(&wt.to_string(), DEFAULT_MODEL_SPACE_ID);
    switch(
        state,
        &BruteForceProjectionStore::new(),
        &shard_dir,
        ShardParams::with_dimensions(DIMS),
        wt,
        genr,
        ms,
        &FakeVectors,
        &SeqUuidV7::new(),
        1000,
    )
    .await
    .expect("establish active tuple via a real switch()");

    let store_uuid = state
        .writer()
        .transaction(|tx| ensure_store_instance_uuid(tx, "cli-doctor-instance"))
        .await
        .expect("ensure store_instance_uuid");
    let cache = local_rag_store::CacheDb::open(layout.cache_db(), &store_uuid)
        .expect("open+bind cache.sqlite");
    materialize_fts(state, &cache, &wt.to_string(), &genr.to_string(), 1000)
        .await
        .expect("materialize fts");
    cache.close();

    (wt, store_uuid)
}

// ---------------------------------------------------------------------------
// Argument parsing / bootstrap
// ---------------------------------------------------------------------------

#[test]
fn doctor_on_a_fresh_store_is_clean() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("local-rag doctor: clean"), "{text}");
    assert!(text.contains("permissions: ok"), "{text}");
    assert!(text.contains("store not yet initialized"), "{text}");
}

#[test]
fn doctor_rejects_an_unknown_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["doctor", "--bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn doctor_rejects_a_worktree_flag_missing_its_value() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["doctor", "--worktree"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn doctor_with_an_invalid_worktree_id_is_reported() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["doctor", "--worktree", "not-a-uuid"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stderr(&output).contains("not a valid worktree id"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn doctor_json_reports_the_same_cleanliness_as_the_exit_code() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["doctor", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(value["clean"], serde_json::json!(true));
}

// ---------------------------------------------------------------------------
// Lock section
// ---------------------------------------------------------------------------

#[test]
fn lock_corrupt_store_lock_makes_the_report_unclean() {
    let (home, layout) = open_layout();
    local_rag_core::paths::ensure_file_0600(&layout.store_lock()).expect("ensure lock file");
    std::fs::write(layout.store_lock(), b"{not valid json at all").expect("seed torn write");

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains("lock: store.lock exists but could not be parsed"),
        "{text}"
    );
}

/// A lock naming a definitely-dead pid is still `Parsed` — `doctor` does not
/// itself judge daemon liveness (that is `status`'s job, per the module doc);
/// it only surfaces `pid_alive` for a human/JSON reader. The lock section
/// alone must not be what makes this report unclean.
#[test]
fn lock_held_by_a_dead_pid_is_parsed_and_does_not_alone_fail_doctor() {
    let (home, layout) = open_layout();
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn trivial child");
    let dead_pid = child.id();
    child.wait().expect("reap child");

    let owner_json = serde_json::json!({
        "instance_uuid": "long-dead-instance",
        "pid": dead_pid,
        "daemon_version": "0.0.0",
        "started_at": 1_000,
        "ready": true,
        "ready_at": 1_000,
        "socket_path": layout.socket_path().display().to_string(),
    })
    .to_string();
    local_rag_core::paths::ensure_file_0600(&layout.store_lock()).expect("ensure lock file");
    std::fs::write(layout.store_lock(), owner_json).expect("seed dead-owner lock file");

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains(&format!("lock: held by pid {dead_pid}")),
        "{text}"
    );
    assert!(text.contains("alive=false"), "{text}");
}

// ---------------------------------------------------------------------------
// Versions section
// ---------------------------------------------------------------------------

/// Applying only a prefix of the real, production migration set (the exact
/// same recipe `crates/store/tests/migrate.rs` uses with synthetic sets)
/// leaves the rest genuinely pending from the compiled binary's point of
/// view. A second `doctor` run must still see the same pending count —
/// `diagnose_versions` never applies anything (D-027's own sibling
/// guarantee: read-only diagnosis never repairs what it finds).
#[test]
fn versions_pending_is_reported_and_a_second_run_does_not_silently_apply_it() {
    let (home, layout) = open_layout();
    {
        let mut conn = rusqlite::Connection::open(layout.state_db()).expect("open state db");
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .expect("busy_timeout");
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .expect("enable WAL");
        let prefix = &local_rag_store::migrate::ALL[..local_rag_store::migrate::ALL.len() - 2];
        local_rag_store::migrate::run(&mut conn, prefix, &layout.migration_lock(), 1000)
            .expect("apply a prefix of the real migration set");
    }

    let first = run_cli(&home, &["doctor"]);
    assert_eq!(first.status.code(), Some(1), "{first:?}");
    let text = stdout(&first);
    assert!(text.contains("2 pending"), "{text}");

    let second = run_cli(&home, &["doctor"]);
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    assert!(
        stdout(&second).contains("2 pending"),
        "a second run must still see the same pending set, not an already-applied one: {}",
        stdout(&second)
    );
}

// ---------------------------------------------------------------------------
// Permissions section
// ---------------------------------------------------------------------------

#[test]
fn permissions_widened_directory_is_reported() {
    let (home, layout) = open_layout();
    std::fs::set_permissions(
        layout.spool_dir(),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("widen spool dir mode");

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("permissions:"), "{text}");
    assert!(text.contains("0755"), "{text}");
    assert!(text.contains("0700"), "{text}");
}

/// Regression on the fixed call order the module doc documents: even on a
/// fully valid, non-pending store — where `build_report` goes on to open a
/// real `StateDb`/`CacheDb` and run the orphans/heads sections after the
/// permissions audit — the fault captured at step 2 is still what is
/// rendered. Guards against a future reordering silently losing the finding
/// to some later step's own re-assert.
#[tokio::test]
async fn permissions_fault_survives_even_though_later_sections_run() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_indexed_worktree(&state, &layout, 1).await;
    }
    std::fs::set_permissions(
        layout.spool_dir(),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("widen spool dir mode");

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("0755"), "{text}");
    // The later sections genuinely ran (not `Skipped`) — the store was valid
    // and non-pending, so this is a real regression guard, not a vacuous one.
    assert!(text.contains("versions: up to date"), "{text}");
    assert!(!text.contains("orphans: skipped"), "{text}");
    assert!(!text.contains("heads: skipped"), "{text}");
}

// ---------------------------------------------------------------------------
// Orphans section
// ---------------------------------------------------------------------------

#[test]
fn orphan_shard_dir_is_reported_via_dry_run_and_not_removed() {
    let (home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("open state.sqlite");
    let dir = layout.projection_shard("orphan-shard");
    std::fs::create_dir_all(&dir).expect("mkdir orphan shard");
    std::fs::write(dir.join("segment.bin"), b"x").expect("seed file");

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("orphans: 1 orphan shard dir(s)"), "{text}");
    assert!(dir.is_dir(), "doctor must never remove what it finds");
}

// ---------------------------------------------------------------------------
// Heads section + `--worktree` filter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worktree_filter_narrows_to_one_and_an_unknown_id_is_a_finding_not_a_crash() {
    let (home, layout) = open_layout();
    let (wt_a, wt_b) = {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let a = seed_bare_worktree(&state, &layout, 1).await;
        let b = seed_bare_worktree(&state, &layout, 10).await;
        (a, b)
    };

    let both = run_cli(&home, &["doctor"]);
    assert_eq!(both.status.code(), Some(0), "{both:?}");
    let text = stdout(&both);
    assert!(text.contains(&wt_a.to_string()), "{text}");
    assert!(text.contains(&wt_b.to_string()), "{text}");

    let narrowed = run_cli(&home, &["doctor", "--worktree", &wt_a.to_string()]);
    assert_eq!(narrowed.status.code(), Some(0), "{narrowed:?}");
    let text = stdout(&narrowed);
    assert!(text.contains(&wt_a.to_string()), "{text}");
    assert!(!text.contains(&wt_b.to_string()), "{text}");

    let unknown_id = uuid(200).to_string();
    let unknown = run_cli(&home, &["doctor", "--worktree", &unknown_id]);
    // A well-formed but never-registered id is a reported finding (an "error:"
    // string on that worktree's line), never a panic/crash.
    assert_eq!(unknown.status.code(), Some(1), "{unknown:?}");
    let text = stdout(&unknown);
    assert!(text.contains(&unknown_id), "{text}");
    assert!(text.contains("error:"), "{text}");
}

#[tokio::test]
async fn dense_head_missing_after_manual_shard_deletion_is_detected_and_the_dir_stays_absent() {
    let (home, layout) = open_layout();
    let (wt, shard_dir) = {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let (wt, _uuid) = seed_indexed_worktree(&state, &layout, 1).await;
        let shard_dir = layout.projection_shard_space(&wt.to_string(), DEFAULT_MODEL_SPACE_ID);
        (wt, shard_dir)
    };
    assert!(shard_dir.is_dir(), "sanity: switch() built a real shard");

    let clean = run_cli(&home, &["doctor"]);
    assert_eq!(clean.status.code(), Some(0), "{clean:?}");

    std::fs::remove_dir_all(&shard_dir).expect("simulate a lost shard directory");

    let broken = run_cli(&home, &["doctor", "--worktree", &wt.to_string()]);
    assert_eq!(broken.status.code(), Some(1), "{broken:?}");
    let text = stdout(&broken);
    assert!(text.contains("HeadMissing"), "{text}");
    assert!(
        !shard_dir.exists(),
        "doctor must never recreate the directory it is diagnosing"
    );
}

#[tokio::test]
async fn fts_unavailable_after_cache_deletion_is_detected_then_recovers_solely_from_state() {
    let (home, layout) = open_layout();
    let wt = {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let (wt, _uuid) = seed_indexed_worktree(&state, &layout, 1).await;
        wt
    };

    let clean = run_cli(&home, &["doctor"]);
    assert_eq!(clean.status.code(), Some(0), "{clean:?}");

    std::fs::remove_file(layout.cache_db()).expect("simulate a lost cache.sqlite");
    for sidecar in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{sidecar}", layout.cache_db().display()));
    }

    let broken = run_cli(&home, &["doctor"]);
    assert_eq!(broken.status.code(), Some(1), "{broken:?}");
    assert!(
        stdout(&broken).contains("cache: not yet initialized"),
        "{}",
        stdout(&broken)
    );

    // Recovery via the already-tested `rebuild --fts` (T15-07, unchanged by
    // this task) — re-derives everything solely from `state.sqlite`'s own
    // occurrences, no embedder involved.
    let rebuild = run_cli(&home, &["rebuild", "--worktree", &wt.to_string(), "--fts"]);
    assert_eq!(rebuild.status.code(), Some(0), "{rebuild:?}");

    let recovered = run_cli(&home, &["doctor"]);
    assert_eq!(recovered.status.code(), Some(0), "{recovered:?}");
}

#[tokio::test]
async fn both_legs_unavailable_is_flagged_when_dense_and_fts_are_both_broken() {
    let (home, layout) = open_layout();
    let (wt, shard_dir) = {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        let (wt, _uuid) = seed_indexed_worktree(&state, &layout, 1).await;
        let shard_dir = layout.projection_shard_space(&wt.to_string(), DEFAULT_MODEL_SPACE_ID);
        (wt, shard_dir)
    };
    std::fs::remove_dir_all(&shard_dir).expect("break dense");
    std::fs::remove_file(layout.cache_db()).expect("break fts");
    for sidecar in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{sidecar}", layout.cache_db().display()));
    }

    let output = run_cli(&home, &["doctor", "--worktree", &wt.to_string()]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stdout(&output).contains("BOTH LEGS UNAVAILABLE"),
        "{}",
        stdout(&output)
    );
}

// ---------------------------------------------------------------------------
// Spool section (D-030)
// ---------------------------------------------------------------------------

/// A genuinely corrupt spool segment (spec 11 §4 `[FIXED concern]`: "a newer
/// hook binary writing a newer format... is a reportable incompatibility, not
/// silent loss") is surfaced by `doctor`, distinctly from a healthy session,
/// and every other section still runs normally (the fix this test guards
/// against is D-030: the daemon's own startup resume pass used to compute
/// this exact signal and then discard it unread).
#[tokio::test]
async fn spool_stall_is_reported_alongside_a_healthy_session() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        // Bring cache binding / code_raw representation to a clean state so
        // the spool section is the *only* thing that can make this unclean.
        seed_bare_worktree(&state, &layout, 1).await;
    }

    let healthy_dir = layout.spool_session("healthy-session");
    std::fs::create_dir_all(&healthy_dir).expect("mkdir healthy session");
    std::fs::write(
        healthy_dir.join("000001.seg"),
        local_rag_core::spool::encode_segment_header(),
    )
    .expect("write a well-formed, empty segment");

    let stalled_dir = layout.spool_session("stalled-session");
    std::fs::create_dir_all(&stalled_dir).expect("mkdir stalled session");
    // 16 zero bytes: exactly HEADER_LEN (never `Truncated`), but the magic
    // does not match — genuine corruption, not a normal in-progress write.
    std::fs::write(stalled_dir.join("000001.seg"), [0u8; 16]).expect("write corrupt header");

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("spool: healthy-session ok"), "{text}");
    assert!(text.contains("spool: stalled-session STALLED"), "{text}");
    assert!(text.contains("magic"), "{text}");
    // Every other section genuinely ran and is clean — this is a spool-only
    // fault, not a knock-on effect of some other section skipping.
    assert!(text.contains("versions: up to date"), "{text}");
    assert!(text.contains("cache: bound"), "{text}");
    assert!(!text.contains("orphans: skipped"), "{text}");
    assert!(!text.contains("heads: skipped"), "{text}");

    let json_output = run_cli(&home, &["doctor", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&json_output.stdout).expect("valid json");
    assert_eq!(value["clean"], serde_json::json!(false));
    let spool = value["spool"].as_array().expect("spool is a json array");
    assert_eq!(spool.len(), 2);
    let stalled = spool
        .iter()
        .find(|s| s["session_id"] == "stalled-session")
        .expect("stalled-session present");
    assert!(stalled["stalled_on"].is_string(), "{stalled:?}");
    let healthy = spool
        .iter()
        .find(|s| s["session_id"] == "healthy-session")
        .expect("healthy-session present");
    assert!(healthy["stalled_on"].is_null(), "{healthy:?}");
}

/// Real end-to-end runs through the compiled binary with the real default
/// model — see `tests/cli_rebuild.rs`'s own `with_real_model` module doc for
/// why this is env-gated: the real dense recovery path (`rebuild --dense`)
/// reads vectors already sitting in `embedding_cache`, which only a genuine
/// `local-rag index` run (ONNX) can have populated correctly.
mod with_real_model {
    use super::*;

    fn require_env() -> Option<(String, String)> {
        let dylib = std::env::var("ORT_DYLIB_PATH").ok();
        let model_home = std::env::var("LOCAL_RAG_TEST_MODEL_HOME").ok();
        match (dylib, model_home) {
            (Some(d), Some(m)) => Some((d, m)),
            _ => {
                eprintln!(
                    "SKIP: ORT_DYLIB_PATH and/or LOCAL_RAG_TEST_MODEL_HOME are unset — \
                     set both to run the real-model doctor recovery tests."
                );
                None
            }
        }
    }

    fn install_real_model(layout: &StoreLayout, model_home: &str) {
        let src = Path::new(model_home)
            .join("models")
            .join(local_rag_models::DEFAULT_MODEL_ID);
        assert!(
            src.join(".ok").is_file(),
            "{}: LOCAL_RAG_TEST_MODEL_HOME must already have the default model installed",
            src.display()
        );
        let dst = layout.model_dir(local_rag_models::DEFAULT_MODEL_ID);
        std::fs::create_dir_all(dst.parent().expect("models dir has a parent"))
            .expect("create models/ parent");
        std::os::unix::fs::symlink(&src, &dst).expect("symlink installed model");
    }

    fn run_cli_with_ort(home: &TempHome, dylib: &str, args: &[&str]) -> Output {
        let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
        cmd.args(args);
        cmd.env("ORT_DYLIB_PATH", dylib);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.output().expect("run local-rag")
    }

    fn worktree_id(layout: &StoreLayout) -> String {
        let conn = rusqlite::Connection::open_with_flags(
            layout.state_db(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open state.sqlite read-only");
        conn.query_row("SELECT worktree_id FROM worktree LIMIT 1", [], |r| r.get(0))
            .expect("one worktree exists")
    }

    #[test]
    fn dense_recovers_via_rebuild_dense_solely_from_state() {
        let Some((dylib, model_home)) = require_env() else {
            return;
        };
        let (home, layout) = open_layout();
        install_real_model(&layout, &model_home);
        let init = run_cli_with_ort(&home, &dylib, &["init"]);
        assert_eq!(init.status.code(), Some(0), "{init:?}");

        let target = home.join("project");
        std::fs::create_dir_all(&target).expect("create target dir");
        std::fs::write(target.join("main.rs"), "fn one() {}").expect("seed file");
        let index = run_cli_with_ort(&home, &dylib, &["index", target.to_str().unwrap()]);
        assert_eq!(index.status.code(), Some(0), "{index:?}");

        let clean = run_cli_with_ort(&home, &dylib, &["doctor"]);
        assert_eq!(clean.status.code(), Some(0), "{clean:?}");

        let wt = worktree_id(&layout);
        let shard_dir = layout.projection_shard_space(&wt, DEFAULT_MODEL_SPACE_ID);
        std::fs::remove_dir_all(&shard_dir).expect("simulate a lost shard directory");

        let broken = run_cli_with_ort(&home, &dylib, &["doctor", "--worktree", &wt]);
        assert_eq!(broken.status.code(), Some(1), "{broken:?}");
        assert!(
            stdout(&broken).contains("HeadMissing"),
            "{}",
            stdout(&broken)
        );
        assert!(!shard_dir.exists());

        let rebuild = run_cli_with_ort(&home, &dylib, &["rebuild", "--worktree", &wt, "--dense"]);
        assert_eq!(rebuild.status.code(), Some(0), "{rebuild:?}");

        let recovered = run_cli_with_ort(&home, &dylib, &["doctor"]);
        assert_eq!(recovered.status.code(), Some(0), "{recovered:?}");
    }

    #[test]
    fn repeated_rebuild_is_idempotent_and_doctor_stays_clean() {
        let Some((dylib, model_home)) = require_env() else {
            return;
        };
        let (home, layout) = open_layout();
        install_real_model(&layout, &model_home);
        let init = run_cli_with_ort(&home, &dylib, &["init"]);
        assert_eq!(init.status.code(), Some(0), "{init:?}");

        let target = home.join("project");
        std::fs::create_dir_all(&target).expect("create target dir");
        std::fs::write(target.join("main.rs"), "fn one() {}\nfn two() {}").expect("seed file");
        let index = run_cli_with_ort(&home, &dylib, &["index", target.to_str().unwrap()]);
        assert_eq!(index.status.code(), Some(0), "{index:?}");

        let wt = worktree_id(&layout);
        for _ in 0..2 {
            let rebuild = run_cli_with_ort(
                &home,
                &dylib,
                &["rebuild", "--worktree", &wt, "--fts", "--dense"],
            );
            assert_eq!(rebuild.status.code(), Some(0), "{rebuild:?}");
            let doctor = run_cli_with_ort(&home, &dylib, &["doctor"]);
            assert_eq!(doctor.status.code(), Some(0), "{doctor:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// X-008: the indexing section, and the one state in it that is a fault
// ---------------------------------------------------------------------------

/// A healthy worktree that nobody enrolled: the section must say so — that is
/// the single most common reason someone believes indexing is broken — while
/// the verdict stays `clean`, because "not enrolled" is a choice, not a fault.
#[tokio::test]
async fn indexing_section_reports_an_unenrolled_worktree_without_failing_the_verdict() {
    let (home, layout) = open_layout();
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (wt, _) = seed_indexed_worktree(&state, &layout, 40).await;
    drop(state);

    let out = run_cli(&home, &["doctor"]);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "an unenrolled worktree is not a fault: {text}{}",
        stderr(&out)
    );
    assert!(
        text.contains("doctor: clean"),
        "verdict stays clean: {text}"
    );
    assert!(
        text.contains(&format!("indexing: {wt}")),
        "the worktree must appear in the indexing section: {text}"
    );
    assert!(
        text.contains("NOT ENROLLED"),
        "and be named as unenrolled: {text}"
    );
    assert!(
        text.contains("serving generation #1, built"),
        "with the age of what it actually serves: {text}"
    );
}

/// The fault X-008 introduces: a generation newer than the active one, built and
/// then never switched on. That is work the system did and dropped — the exact
/// shape the reporter's live store was in (#3308/#3309 behind an active #3307) —
/// and it must both print loudly and fail the verdict.
#[tokio::test]
async fn a_generation_built_but_never_activated_is_reported_and_fails_the_verdict() {
    let (home, layout) = open_layout();
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (wt, _) = seed_indexed_worktree(&state, &layout, 44).await;

    // A second, newer generation that reaches `projection_ready` and stops
    // there — no `switch()`, so the worktree keeps serving the first one.
    let stuck = uuid(200);
    let (w, g) = (wt.to_string(), stuck.to_string());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 2000).map(|_| ()))
        .await
        .expect("allocate the newer generation");
    let g2 = stuck.to_string();
    state
        .writer()
        .transaction(move |tx| transition_generation(tx, &g2, GenerationState::ProjectionReady))
        .await
        .expect("transition tx")
        .expect("building -> projection_ready is legal");
    drop(state);

    let out = run_cli(&home, &["doctor"]);
    let text = stdout(&out);
    assert!(
        !out.status.success(),
        "built-but-unserved work must fail the verdict: {text}"
    );
    assert!(
        text.contains("doctor: issues found"),
        "and say so in the headline: {text}"
    );
    assert!(
        text.contains("STUCK: generation #2 is projection_ready but never became active"),
        "the stuck generation must be named with its number and state: {text}"
    );
}

/// `--json` carries the same finding, including the machine-readable
/// `stuck_generations` list and the `clean: false` verdict.
#[tokio::test]
async fn indexing_json_carries_enrollment_freshness_and_stuck_generations() {
    let (home, layout) = open_layout();
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (wt, _) = seed_indexed_worktree(&state, &layout, 48).await;
    let stuck = uuid(210);
    let (w, g) = (wt.to_string(), stuck.to_string());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, 2000).map(|_| ()))
        .await
        .expect("allocate the newer generation");
    let g2 = stuck.to_string();
    state
        .writer()
        .transaction(move |tx| transition_generation(tx, &g2, GenerationState::ProjectionReady))
        .await
        .expect("transition tx")
        .expect("building -> projection_ready is legal");
    drop(state);

    let out = run_cli(&home, &["doctor", "--json"]);
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("doctor --json emits valid JSON");
    assert_eq!(json["clean"], serde_json::Value::Bool(false));

    let entry = json["indexing"]
        .as_array()
        .expect("indexing is an array of worktrees")
        .iter()
        .find(|e| e["worktree_id"] == wt.to_string())
        .expect("the seeded worktree appears")
        .clone();
    assert_eq!(entry["managed"], serde_json::Value::Bool(false));
    assert_eq!(entry["active_generation_number"], serde_json::json!(1));
    assert_eq!(
        entry["stuck_generations"][0]["generation_number"],
        serde_json::json!(2)
    );
    assert_eq!(
        entry["stuck_generations"][0]["state"],
        serde_json::json!("projection_ready")
    );
}

// ---------------------------------------------------------------------------
// D-071: the consolidation section
// ---------------------------------------------------------------------------

/// A `consolidation_run` that failed `attempts` times and ended dead-lettered
/// on the running build — the D-069 incident's shape, driven through the real
/// `retry_run`/`record_run_failure` cycle.
async fn seed_stuck_run(db: &StateDb, run_id: &str, session_id: &str, attempts: i64) {
    let (rid, sid) = (run_id.to_string(), session_id.to_string());
    db.writer()
        .transaction(move |tx| {
            create_consolidation_run(
                tx,
                &NewConsolidationRun {
                    run_id: &rid,
                    session_id: &sid,
                    from_received_seq: 26115,
                    to_received_seq: 26117,
                    router_version: "v1",
                },
                1_000,
            )?;
            for attempt in 0..attempts {
                if attempt == 0 {
                    transition_run(tx, &rid, RunState::Running, 1_000)?
                        .expect("pending -> running");
                } else {
                    retry_run(tx, &rid, LEASE_DURATION_MS, 1_000)?.expect("failed -> running");
                }
                record_run_failure(
                    tx,
                    &rid,
                    FailureKind::Mechanical,
                    "state transaction failed (rolled back): UNIQUE constraint failed: \
                     candidate_evidence.candidate_id, candidate_evidence.observation_id",
                    false,
                    Some(local_rag_core::BUILD_ID),
                    1_000,
                )?
                .expect("running -> failed");
            }
            Ok(())
        })
        .await
        .expect("seed stuck run");
}

/// `doctor` had no consolidation section at all, so it reported `clean` while
/// one run was being retried into the ground — the whole point of D-071. A
/// dead-lettered run is a fault for the same reason a stuck generation is:
/// work the system performed and could not land, and here it additionally
/// blocks its session's whole backlog until the binary is rebuilt.
#[tokio::test]
async fn consolidation_section_reports_a_dead_lettered_run_and_fails_the_verdict() {
    let (home, layout) = open_layout();
    {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        bootstrap_store(&db, &layout).await;
        seed_stuck_run(&db, "run-wedged", "sess-wedged", 627).await;
    }

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a dead-lettered run must fail the verdict: {output:?}"
    );
    let text = stdout(&output);
    assert!(text.contains("local-rag doctor: issues found"), "{text}");
    assert!(
        text.contains("consolidation: run run-wedged session sess-wedged"),
        "{text}"
    );
    assert!(text.contains("627 attempt(s)"), "{text}");
    assert!(text.contains("DEAD-LETTERED on this build"), "{text}");
    assert!(
        text.contains("UNIQUE constraint failed: candidate_evidence"),
        "the recorded failure is what tells an operator what to fix: {text}"
    );

    let output = run_cli(&home, &["doctor", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(json["clean"], serde_json::json!(false));
    let stuck = json["consolidation"]["stuck_runs"].as_array().unwrap();
    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0]["run_id"], "run-wedged");
    assert_eq!(stuck[0]["attempt_count"], 627);
    assert_eq!(stuck[0]["dead_lettered"], true);
}

/// The healthy counterpart: an initialized store with nothing wedged says so
/// explicitly (an `ok` line, like `permissions:`) and stays clean.
#[tokio::test]
async fn consolidation_section_is_ok_when_no_run_is_stuck() {
    let (home, layout) = open_layout();
    {
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        bootstrap_store(&db, &layout).await;
    }

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("local-rag doctor: clean"), "{text}");
    assert!(
        text.contains("consolidation: ok — no run is stuck or dead-lettered"),
        "{text}"
    );
}

// ---------------------------------------------------------------------------
// T21-08: normalization + generator sections (ADR-0010)
// ---------------------------------------------------------------------------

/// A store `doctor` reports as clean: migrated, with a bound cache — the same
/// `bootstrap_store` every other section's tests use here.
async fn initialize_store(layout: &StoreLayout) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    bootstrap_store(&db, layout).await;
}

fn write_config(home: &TempHome, body: &str) {
    let dir = home.join("config");
    std::fs::create_dir_all(&dir).expect("mk config dir");
    std::fs::write(dir.join("config.toml"), body).expect("write config.toml");
}

/// The `.ok` marker the installer writes last (spec 10 §5) — and **only** it.
/// A `doctor` that reported "installed" here while loading nothing is exactly
/// the behaviour under test; a `doctor` that tried to load would fail on the
/// missing GGUF.
fn fake_installed_generator(layout: &StoreLayout) {
    let dir = layout.model_dir(local_rag_generate::DEFAULT_MODEL_ID);
    std::fs::create_dir_all(&dir).expect("mk model dir");
    std::fs::write(dir.join(".ok"), b"").expect("write .ok marker");
}

async fn seed_entry_with_normalization(
    layout: &StoreLayout,
    memory_id: &str,
    text: &str,
    status: Option<(NormalizationStatus, i64)>,
) {
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let (id, body) = (memory_id.to_string(), text.to_string());
    state
        .writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: &body,
                    canonical_key: None,
                    scope_kind: ScopeKind::Global,
                    scope_owner_id: GLOBAL_SCOPE_OWNER_ID,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                1_000,
            )
        })
        .await
        .expect("seed entry tx")
        .expect("seed entry domain");

    let Some((status, attempt_count)) = status else {
        return;
    };
    let (id, sha) = (
        memory_id.to_string(),
        local_rag_core::hash::sha256_hex(text.as_bytes()),
    );
    let outcome = state
        .writer()
        .transaction(move |tx| {
            upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status,
                    source_text_sha256: &sha,
                    normalized_text: match status {
                        NormalizationStatus::Ready => Some("the English variant"),
                        _ => None,
                    },
                    source_language: Some("ru"),
                    normalizer_model_id: Some("test-normalizer"),
                    prompt_version: Some(1),
                    normalizer_version: CURRENT_NORMALIZER_VERSION,
                    attempt_count,
                    last_error: match status {
                        NormalizationStatus::Failed => Some("answer was not one object"),
                        _ => None,
                    },
                    next_attempt_at: None,
                },
                2_000,
            )
        })
        .await
        .expect("seed normalization tx");
    assert_eq!(outcome, UpsertOutcome::Written);
}

#[tokio::test]
async fn normalization_section_reports_the_switch_being_off() {
    let (home, layout) = open_layout();
    initialize_store(&layout).await;
    seed_entry_with_normalization(&layout, "mem-ru", "запись по-русски", None).await;
    write_config(&home, "[memory]\nnormalize_to_english = false\n");

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "off is a choice, not a fault"
    );
    let text = stdout(&output);
    assert!(text.contains("normalization: OFF"), "{text}");
    assert!(text.contains("normalize_to_english = false"), "{text}");
    assert!(
        text.contains("1 pending"),
        "the backlog is still reported while off: {text}",
    );

    let json: serde_json::Value =
        serde_json::from_slice(&run_cli(&home, &["doctor", "--json"]).stdout).expect("valid json");
    assert_eq!(json["normalization"]["enabled"], serde_json::json!(false));
    assert_eq!(json["normalization"]["pending"], 1);
    assert_eq!(json["clean"], serde_json::json!(true));
}

#[tokio::test]
async fn normalization_section_is_clean_when_on_with_nothing_wrong() {
    let (home, layout) = open_layout();
    initialize_store(&layout).await;
    seed_entry_with_normalization(
        &layout,
        "mem-ru",
        "запись по-русски",
        Some((NormalizationStatus::Ready, 1)),
    )
    .await;
    // T21-11 flipped the default to `false`, so "on" is now stated rather than
    // inherited — this test is about how the section *renders* when the worker
    // is on, and must not silently become the off-case if the default moves
    // again.
    write_config(&home, "[memory]\nnormalize_to_english = true\n");

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("normalization: on"), "{text}");
    assert!(text.contains("0 pending"), "{text}");
    assert!(text.contains("ready: 1"), "{text}");
    assert!(!text.contains("DEAD-LETTERED"), "{text}");
}

#[tokio::test]
async fn a_dead_lettered_entry_makes_the_report_unclean_and_names_itself() {
    let (home, layout) = open_layout();
    initialize_store(&layout).await;
    seed_entry_with_normalization(
        &layout,
        "mem-ru",
        "непереводимая запись",
        Some((NormalizationStatus::Failed, MAX_NORMALIZATION_ATTEMPTS)),
    )
    .await;

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "work the normalizer gave up on is a fault, like a stuck run",
    );
    let text = stdout(&output);
    assert!(text.contains("DEAD-LETTERED: 1 entry(ies)"), "{text}");
    assert!(text.contains("mem-ru after 5 attempt(s)"), "{text}");
    assert!(text.contains("answer was not one object"), "{text}");

    let json: serde_json::Value =
        serde_json::from_slice(&run_cli(&home, &["doctor", "--json"]).stdout).expect("valid json");
    assert_eq!(json["clean"], serde_json::json!(false));
    assert_eq!(json["normalization"]["dead_letter"], 1);
    assert_eq!(
        json["normalization"]["dead_letters"][0]["memory_id"],
        "mem-ru"
    );
}

#[tokio::test]
async fn the_detector_limitation_is_printed_on_every_healthy_run() {
    let (home, layout) = open_layout();
    initialize_store(&layout).await;

    let text = stdout(&run_cli(&home, &["doctor"]));
    assert!(
        text.contains("detects scripts, not languages"),
        "the declared limitation is permanent, not conditional: {text}",
    );
    assert!(text.contains("German, French, Spanish, Polish"), "{text}");

    let json: serde_json::Value =
        serde_json::from_slice(&run_cli(&home, &["doctor", "--json"]).stdout).expect("valid json");
    assert!(
        json["normalization"]["detector_limitation"]
            .as_str()
            .is_some_and(|s| s.contains("scripts, not languages")),
    );
}

#[tokio::test]
async fn generator_section_reports_a_model_that_is_not_installed() {
    let (home, layout) = open_layout();
    initialize_store(&layout).await;

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "an uninstalled model is a bootstrap state, not a fault",
    );
    let text = stdout(&output);
    assert!(text.contains("NOT INSTALLED"), "{text}");
    assert!(text.contains("init --download-models"), "{text}");

    let json: serde_json::Value =
        serde_json::from_slice(&run_cli(&home, &["doctor", "--json"]).stdout).expect("valid json");
    assert_eq!(json["generator"]["installed"], serde_json::json!(false));
    assert_eq!(json["generator"]["catalogued"], serde_json::json!(true));
    assert_eq!(
        json["generator"]["model_id"],
        local_rag_generate::DEFAULT_MODEL_ID
    );
    assert_eq!(json["clean"], serde_json::json!(true));
}

/// The whole point of the file-only check: an `.ok` marker with **no weights
/// whatsoever** reports "installed", which is only possible because nothing
/// here opens `LlamaBackend` (a process-wide singleton, D-054) or reads a
/// single byte of GGUF.
#[tokio::test]
async fn generator_section_reports_installed_from_the_marker_alone() {
    let (home, layout) = open_layout();
    initialize_store(&layout).await;
    fake_installed_generator(&layout);

    let output = run_cli(&home, &["doctor"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains(&format!(
            "generator: {} installed",
            local_rag_generate::DEFAULT_MODEL_ID
        )),
        "{text}",
    );

    let json: serde_json::Value =
        serde_json::from_slice(&run_cli(&home, &["doctor", "--json"]).stdout).expect("valid json");
    assert_eq!(json["generator"]["installed"], serde_json::json!(true));
}

/// A store `doctor` cannot open yet reports the sections it *can*: whether the
/// model is on disk is a filesystem fact and does not depend on SQLite.
#[test]
fn the_generator_section_answers_even_on_an_uninitialized_store() {
    let (home, layout) = open_layout();
    fake_installed_generator(&layout);

    let json: serde_json::Value =
        serde_json::from_slice(&run_cli(&home, &["doctor", "--json"]).stdout).expect("valid json");
    assert_eq!(json["generator"]["installed"], serde_json::json!(true));
    assert!(
        json["normalization"]["skipped"].is_string(),
        "the store-backed half honestly says it could not look",
    );
    assert_eq!(json["clean"], serde_json::json!(true));
}
