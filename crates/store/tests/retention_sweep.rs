//! T06-02 crash/resume acceptance tests for the batched sweep (spec 06 §5; 03 §3).
//! Compiled only under the `failpoints` feature: they arm the sweep's
//! `retention.sweep.between_batches` injection point to interrupt the sweep with
//! real partial progress committed to disk, then assert that simply re-running
//! `run_sweep` resumes and converges — the sweep needs no separate progress
//! checkpoint because each batch is its own committed transaction and the
//! sweepable sets are recomputed from the live database on every call.
//!
//! Two interruption models:
//!
//! - `interruption_between_batches_resumes` — an in-process `Action::Error` after
//!   the first committed batch; the same process disarms and resumes, then a third
//!   run proves idempotence (a no-op);
//! - `resumable_hard_kill_via_sigabrt` — a genuine `SIGABRT` mid-sweep in a child
//!   process (power-loss model), with the parent resuming in a fresh process.
//!
//! The injection point lives in a **process-global** registry, so these tests hold
//! [`SERIAL`] and reset the registry on entry; other integration test binaries are
//! separate processes and never share it.
//!
//! Determinism: an isolated [`TempHome`], fixed `now_ms`, ids with fixed entropy —
//! no wall clock, no network, no sleeps.
#![cfg(feature = "failpoints")]

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    GenerationState, WorktreeKind, allocate_generation, create_repository, create_worktree,
    transition_generation,
};
use local_rag_store::{
    EdgeResolution, ExternalPins, NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit,
    NewResolvedEdge, NewlineStyle, RetentionParams, SourceCompression, StateDb, SweepError,
    UnitKind, insert_content_blob, insert_file_revision, insert_generation_file, insert_occurrence,
    insert_parsed_unit, insert_resolved_edge, run_sweep, run_sweep_with_batch,
};
use local_rag_test_support::TempHome;
use local_rag_test_support::failpoint::{Action, global};
use tokio::sync::{Mutex, MutexGuard};

/// The sweep's between-batch injection point.
const FP: &str = "retention.sweep.between_batches";

/// Serializes tests that touch the process-global failpoint registry, resetting it
/// on entry so no arming leaks between tests.
static SERIAL: Mutex<()> = Mutex::const_new(());

async fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().await;
    global().reset();
    guard
}

/// K=0 / T=0: every `retiring`/`failed` generation is a sweep candidate.
fn params() -> RetentionParams {
    RetentionParams {
        keep_last_k: 0,
        window_ms: 0,
    }
}

/// A distinct, deterministic UUIDv7 keyed by `seed` (no clock/entropy).
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Read a scalar `i64` over a fresh read connection.
fn scalar(db: &StateDb, sql: &str) -> i64 {
    let conn = db.open_read().expect("read conn");
    conn.query_row(sql, [], |r| r.get(0)).expect("scalar query")
}

/// Rows in `table`.
fn rows(db: &StateDb, table: &str) -> i64 {
    scalar(db, &format!("SELECT COUNT(*) FROM {table}"))
}

/// Create a repository + one `active` main worktree; returns `worktree_id`.
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

/// Allocate a `building` generation and drive it to `retiring`; returns its id.
async fn retiring_generation(db: &StateDb, worktree_id: &str, seed: u8) -> String {
    let g = uuid(seed);
    let (w, gid) = (worktree_id.to_string(), g.clone());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &gid, 1000).map(|_| ()))
        .await
        .expect("allocate generation");
    for to in [
        GenerationState::ProjectionReady,
        GenerationState::Active,
        GenerationState::Retiring,
    ] {
        let gid = g.clone();
        db.writer()
            .transaction(move |tx| transition_generation(tx, &gid, to))
            .await
            .expect("transition tx")
            .expect("legal transition");
    }
    g
}

/// Seed a wholly-unpinned (`retiring`) generation with a multi-row content graph:
/// one revision + blob + unit, three membership paths, three occurrences and three
/// edges — enough rows that a batch ceiling of one interrupts mid-phase with real
/// partial progress. Returns the generation id.
async fn seed_candidate(db: &StateDb, worktree_id: &str, seed: u8) -> String {
    let g = retiring_generation(db, worktree_id, seed).await;

    // Content side (shared, path-independent).
    {
        let g = g.clone();
        db.writer()
            .transaction(move |tx| {
                insert_content_blob(
                    tx,
                    &NewContentBlob {
                        blob_id: "blob",
                        language: "rust",
                        algo_version: 1,
                        normalization_version: 1,
                    },
                    1000,
                )?;
                insert_file_revision(
                    tx,
                    &NewFileRevision {
                        file_revision_id: "rev",
                        content_hash: "hash",
                        parser_fingerprint: "fp",
                        source_blob: b"x",
                        compression: SourceCompression::None,
                        source_encoding: "utf-8",
                        newline_style: NewlineStyle::Lf,
                        source_size: 1,
                    },
                    1000,
                )?;
                insert_parsed_unit(
                    tx,
                    &NewParsedUnit {
                        unit_id: "unit",
                        file_revision_id: "rev",
                        unit_kind: UnitKind::Symbol,
                        syntax_locator: "unit",
                        blob_id: "blob",
                        span_start: 0,
                        span_end: 1,
                        local_name: None,
                        kind: None,
                        parent_unit_id: None,
                    },
                )?;
                // Membership + occurrences at three paths.
                for path in ["a.rs", "b.rs", "c.rs"] {
                    insert_generation_file(tx, &g, path, path, "rev")?;
                    insert_occurrence(
                        tx,
                        &NewOccurrence {
                            occurrence_id: &format!("occ-{path}"),
                            generation_id: &g,
                            normalized_path: path,
                            unit_id: "unit",
                            qualified_name: None,
                            context_hash: None,
                        },
                    )?;
                }
                // Three distinct edges among the occurrences.
                let occs = ["occ-a.rs", "occ-b.rs", "occ-c.rs"];
                for (src, dst) in [(0, 1), (1, 2), (2, 0)] {
                    insert_resolved_edge(
                        tx,
                        &NewResolvedEdge {
                            generation_id: &g,
                            src_occurrence_id: occs[src],
                            dst_occurrence_id: occs[dst],
                            edge_kind: "import",
                            resolution: EdgeResolution::Heuristic,
                        },
                    )?;
                }
                Ok(())
            })
            .await
            .expect("seed content graph");
    }
    g
}

/// The graph is fully swept: every table the sweep touches is empty.
fn assert_fully_swept(db: &StateDb) {
    for table in [
        "resolved_graph_edge",
        "generation_unit_occurrence",
        "generation_file",
        "generation",
        "unresolved_reference",
        "parsed_unit",
        "file_revision",
        "content_blob",
    ] {
        assert_eq!(rows(db, table), 0, "{table} fully swept");
    }
}

/// An in-process interruption after the first committed batch leaves real partial
/// progress; disarming and re-running resumes to a full sweep, and a third run is a
/// no-op (idempotence).
#[tokio::test]
async fn interruption_between_batches_resumes() {
    let _guard = serial().await;
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");

    let g = seed_candidate(&db, &worktree(&db, 1).await, 10).await;
    let edges_before = rows(&db, "resolved_graph_edge");
    assert_eq!(edges_before, 3);

    // Arm: fail immediately after the first committed batch.
    global().register(FP);
    global().arm(FP, Action::Error).expect("arm");

    let interrupted = run_sweep_with_batch(&db, &params(), &ExternalPins::default(), 2000, 1).await;
    assert!(
        matches!(interrupted, Err(SweepError::Interrupted)),
        "expected an interrupt, got {interrupted:?}"
    );

    // Partial progress committed: one edge gone, the generation still present.
    assert_eq!(rows(&db, "resolved_graph_edge"), 2, "one batch committed");
    assert_eq!(
        scalar(
            &db,
            &format!("SELECT COUNT(*) FROM generation WHERE generation_id = '{g}'")
        ),
        1,
        "generation not yet swept"
    );

    // Resume: disarm and run to completion.
    global().disarm(FP).expect("disarm");
    let report = run_sweep(&db, &params(), &ExternalPins::default(), 2000)
        .await
        .expect("resume sweep");
    assert!(report.total() > 0, "resume removed the remaining rows");
    assert_fully_swept(&db);

    // Idempotent: a third run finds nothing left.
    let again = run_sweep(&db, &params(), &ExternalPins::default(), 2000)
        .await
        .expect("idempotent re-run");
    assert!(again.is_empty(), "second full run is a no-op: {again:?}");
}

/// A genuine `SIGABRT` mid-sweep (power-loss model) is healed by a resume in a fresh
/// process: the batch committed before the crash stands, and re-running completes
/// the sweep. Gated on `unix` (signal inspection) + `failpoints` (the abort seam).
#[cfg(all(unix, feature = "failpoints"))]
#[test]
fn resumable_hard_kill_via_sigabrt() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    use local_rag_test_support::run_capturing;

    const CHILD_ENV: &str = "LOCAL_RAG_T0602_SIGABRT_CHILD";

    // Child mode: open the pre-seeded store, arm abort between batches, sweep — it
    // dies with SIGABRT right after the first batch commits.
    if let Ok(root) = std::env::var(CHILD_ENV) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("child runtime");
        rt.block_on(async {
            let layout = StoreLayout::new(std::path::PathBuf::from(root));
            let db = StateDb::open(layout.state_db()).expect("child open");
            global().register(FP);
            global().arm(FP, Action::Abort).expect("arm abort");
            // Expected to abort inside the sweep after the first committed batch.
            let _ = run_sweep_with_batch(&db, &params(), &ExternalPins::default(), 2000, 1).await;
            // Reaching here means the seam did not fire — fail loudly (not a signal).
            std::process::exit(97);
        });
        unreachable!("child aborts inside the sweep");
    }

    // Parent mode: seed the store, then spawn the child against it.
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("parent runtime");
    rt.block_on(async {
        let db = StateDb::open(layout.state_db()).expect("open");
        let _g = seed_candidate(&db, &worktree(&db, 1).await, 10).await;
        assert_eq!(rows(&db, "resolved_graph_edge"), 3);
        // Drop `db` here (end of block) so the writer thread flushes and the child
        // opens a quiescent store.
    });

    let mut cmd = Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg("resumable_hard_kill_via_sigabrt")
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, layout.root());
    let outcome = run_capturing(cmd, "t06_02-sigabrt").expect("spawn child");

    assert_eq!(
        outcome.status.signal(),
        Some(6),
        "child must die with SIGABRT; status={:?} bundle={:?}\nstderr:\n{}",
        outcome.status,
        outcome.bundle,
        outcome.stderr_lossy()
    );

    // The batch committed before the crash stands.
    let db = StateDb::open(layout.state_db()).expect("reopen");
    assert_eq!(
        rows(&db, "resolved_graph_edge"),
        2,
        "one batch committed before the hard kill"
    );

    // Resume in this fresh process: the remaining rows are swept.
    let rt2 = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("resume runtime");
    let report = rt2.block_on(async {
        run_sweep(&db, &params(), &ExternalPins::default(), 2000)
            .await
            .expect("resume sweep")
    });
    assert!(report.total() > 0, "resume swept the remaining rows");
    assert_fully_swept(&db);
}
