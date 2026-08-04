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
    DEFAULT_MODEL_SPACE_ID, DistanceMetric, GenerationState, NewContentBlob, NewFileRevision,
    NewOccurrence, NewParsedUnit, NewlineStyle, RepresentationKey, RepresentationKind,
    SourceCompression, StateDb, UnitKind, WorktreeKind, allocate_generation, create_repository,
    create_worktree, derive_content_blob, ensure_store_instance_uuid, insert_content_blob,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    insert_projection_state, materialize_fts, occurrence_id, register_representation,
    set_model_space_representation, transition_generation,
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
