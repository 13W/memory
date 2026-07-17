//! T01-04 acceptance tests: resumable / destructive migration mechanics
//! (spec 13 §3).
//!
//! These drive [`migrate::run`] with **synthetic** complex migrations (Rust
//! steps and/or a destructive marker) so the checkpoint/backup machinery is
//! exercised without the real (empty) production set. All tests are
//! deterministic: an isolated [`TempHome`], fixed `now_ms` literals, and — for
//! crash injection — named failpoints from the shared harness rather than any
//! wall-clock sleep or unstructured kill.
//!
//! # Crash-injection model
//!
//! A migration unit commits its work and its progress row in one transaction, so
//! for WAL + `synchronous=FULL` only committed units survive a crash. The
//! in-process tests model "crash right at the end of unit *k*" by arming a
//! failpoint inside step *k*'s closure to return an error (the runner rolls the
//! unit back exactly as a crash would leave it), then reopening a **fresh
//! connection** to prove the durable state and resume it. One test additionally
//! exercises a genuine hard kill: a re-executed child process aborts (`SIGABRT`)
//! right after the backup checkpoint commits (via the runner's feature-gated
//! `migrate:after_backup` seam), and the parent resumes it.

use std::path::Path;
use std::time::Duration;

use local_rag_core::paths::StoreLayout;
use local_rag_store::migrate::{Migration, MigrationError, MigrationStep, run};
use local_rag_store::rusqlite::{self, Connection, Transaction};
use local_rag_test_support::{Action, TempHome};

// ---- synthetic migrations ---------------------------------------------------

/// A synthetic SQLite error used to model a crash at the end of a unit.
fn injected_error(what: &str) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
        Some(format!("injected crash at {what}")),
    )
}

/// Generate a Rust step that appends its label to `markers` (append-only, so a
/// double execution is detectable) and, when its named failpoint is armed to
/// [`Action::Error`], returns an error *after* doing that work — modelling a
/// crash at the end of this unit (the enclosing transaction rolls the work back).
macro_rules! marker_step {
    ($fn_name:ident, $label:literal, $fp:literal) => {
        fn $fn_name(tx: &Transaction<'_>) -> rusqlite::Result<()> {
            tx.execute("INSERT INTO markers(label) VALUES (?1)", [$label])?;
            local_rag_test_support::fail_point!($fp, Err(injected_error($label)));
            Ok(())
        }
    };
}

// Scenario 1 — resume after each checkpoint (three checkpointed steps).
marker_step!(re_a, "a", "t01_04::resume_each::a");
marker_step!(re_b, "b", "t01_04::resume_each::b");
marker_step!(re_c, "c", "t01_04::resume_each::c");
const RESUME_STEPS: &[MigrationStep] = &[
    MigrationStep {
        label: "a",
        run: re_a,
    },
    MigrationStep {
        label: "b",
        run: re_b,
    },
    MigrationStep {
        label: "c",
        run: re_c,
    },
];
const RESUME_SET: &[Migration] = &[Migration::sql(1, "stepped", "").with_steps(RESUME_STEPS)];
const RESUME_LABELS: [&str; 3] = ["a", "b", "c"];

// Scenario 2 — failed step / retry (distinct failpoint namespace).
marker_step!(fs_a, "a", "t01_04::failstep::a");
marker_step!(fs_b, "b", "t01_04::failstep::b");
marker_step!(fs_c, "c", "t01_04::failstep::c");
const FAILSTEP_STEPS: &[MigrationStep] = &[
    MigrationStep {
        label: "a",
        run: fs_a,
    },
    MigrationStep {
        label: "b",
        run: fs_b,
    },
    MigrationStep {
        label: "c",
        run: fs_c,
    },
];
const FAILSTEP_SET: &[Migration] = &[Migration::sql(1, "stepped", "").with_steps(FAILSTEP_STEPS)];

// Shared seed for destructive-backup scenarios: v1 creates and seeds `data`.
const SEED_V1: Migration = Migration::sql(
    1,
    "seed",
    "CREATE TABLE data(x INTEGER); INSERT INTO data(x) VALUES (42);",
);

/// A plain (no failpoint) destructive wipe step.
fn delete_data(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM data", [])?;
    Ok(())
}

/// A destructive wipe step with an injectable crash (its own failpoint name).
fn wipe_injectable(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM data", [])?;
    local_rag_test_support::fail_point!("t01_04::backup_resume::wipe", Err(injected_error("wipe")));
    Ok(())
}

const WIPE_STEPS: &[MigrationStep] = &[MigrationStep {
    label: "wipe",
    run: delete_data,
}];
const WIPE_INJECTABLE_STEPS: &[MigrationStep] = &[MigrationStep {
    label: "wipe",
    run: wipe_injectable,
}];

// Destructive migration (v2) that wipes `data`, in two flavours.
const BACKUP_V2: Migration = Migration::sql(2, "wipe", "")
    .destructive()
    .with_steps(WIPE_STEPS);
const BACKUP_SET_V1: &[Migration] = &[SEED_V1];
const BACKUP_SET_V12: &[Migration] = &[SEED_V1, BACKUP_V2];

const BACKUP_RESUME_V2: Migration = Migration::sql(2, "wipe", "")
    .destructive()
    .with_steps(WIPE_INJECTABLE_STEPS);
const BACKUP_RESUME_SET: &[Migration] = &[SEED_V1, BACKUP_RESUME_V2];

// Destructive, SQL-only migration (no Rust steps): backup then a DDL unit.
const DESTRUCTIVE_SQL_V2: Migration =
    Migration::sql(2, "add_table", "CREATE TABLE extra(y INTEGER);").destructive();
const DESTRUCTIVE_SQL_SET: &[Migration] = &[SEED_V1, DESTRUCTIVE_SQL_V2];

// ---- helpers ----------------------------------------------------------------

/// A temp store with an ensured tree; returns the home (kept alive for cleanup)
/// and its [`StoreLayout`].
fn temp_store() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// A raw read-write connection with the durability pragmas that matter for the
/// crash model (WAL + `synchronous=FULL`).
fn raw_conn(path: &Path) -> Connection {
    let conn = Connection::open(path).expect("open state db");
    conn.busy_timeout(Duration::from_secs(5))
        .expect("busy_timeout");
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
        .expect("pragmas");
    conn
}

/// Create the append-only scaffolding table the marker steps write to.
fn seed_markers(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS markers \
         (id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT NOT NULL);",
    )
    .expect("create markers");
}

fn arm(name: &str) {
    let fp = local_rag_test_support::failpoint::global();
    fp.register(name);
    fp.arm(name, Action::Error).expect("arm failpoint");
}

fn disarm(name: &str) {
    local_rag_test_support::failpoint::global()
        .disarm(name)
        .expect("disarm failpoint");
}

fn migration_applied(conn: &Connection, version: u32) -> bool {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_migrations WHERE version = ?1",
            [version],
            |r| r.get(0),
        )
        .expect("count schema_migrations");
    n == 1
}

fn progress_seqs(conn: &Connection, version: u32) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT seq FROM migration_progress WHERE version = ?1 ORDER BY seq")
        .expect("prepare progress");
    let rows = stmt
        .query_map([version], |r| r.get::<_, i64>(0))
        .expect("query progress");
    rows.map(|r| r.expect("row")).collect()
}

fn marker_labels(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT label FROM markers ORDER BY id")
        .expect("prepare markers");
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query markers");
    rows.map(|r| r.expect("row")).collect()
}

fn data_values(conn: &Connection) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT x FROM data ORDER BY x")
        .expect("prepare data");
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .expect("query data");
    rows.map(|r| r.expect("row")).collect()
}

/// Read `SELECT x FROM data` from an independent SQLite file (a backup).
fn backup_data_values(path: &Path) -> Vec<i64> {
    let conn = Connection::open(path).expect("open backup");
    data_values(&conn)
}

fn count_backups(layout: &StoreLayout) -> usize {
    std::fs::read_dir(layout.backups_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .count()
        })
        .unwrap_or(0)
}

// ---- tests ------------------------------------------------------------------

/// Crash after each checkpoint resumes exactly: for every step index `k`, arming
/// step `k` to fail leaves units `[0..k)` committed and everything from `k`
/// pending; a fresh reopen resumes to completion with each step's effect applied
/// exactly once.
#[test]
fn resume_after_each_checkpoint() {
    for k in 0..RESUME_LABELS.len() {
        let (_home, layout) = temp_store();
        let name = format!("t01_04::resume_each::{}", RESUME_LABELS[k]);

        // First run: crash at step k.
        {
            let mut conn = raw_conn(&layout.state_db());
            seed_markers(&conn);
            arm(&name);
            let err = run(&mut conn, RESUME_SET, &layout.migration_lock(), 1000)
                .expect_err("armed step must fail the run");
            assert!(
                matches!(err, MigrationError::Sqlite(_)),
                "step failure surfaces as Sqlite, got {err:?}"
            );
            disarm(&name);
            // conn dropped → models a restart.
        }

        // Durable state after the "crash", observed on a fresh connection.
        {
            let conn = raw_conn(&layout.state_db());
            assert_eq!(
                progress_seqs(&conn, 1),
                (0..k as i64).collect::<Vec<_>>(),
                "only units before k are checkpointed (k={k})"
            );
            assert!(
                !migration_applied(&conn, 1),
                "migration not finalized (k={k})"
            );
            assert_eq!(
                marker_labels(&conn),
                RESUME_LABELS[..k].to_vec(),
                "only committed steps left a marker (k={k})"
            );
        }

        // Resume: nothing armed → completes.
        {
            let mut conn = raw_conn(&layout.state_db());
            let report = run(&mut conn, RESUME_SET, &layout.migration_lock(), 2000)
                .expect("resume completes");
            assert_eq!(report.applied, vec![1], "resume applies version 1 (k={k})");
        }

        // Final state: version recorded, progress cleared, each step exactly once.
        {
            let conn = raw_conn(&layout.state_db());
            assert!(migration_applied(&conn, 1));
            assert!(
                progress_seqs(&conn, 1).is_empty(),
                "progress cleared (k={k})"
            );
            assert_eq!(
                marker_labels(&conn),
                RESUME_LABELS.to_vec(),
                "each step applied exactly once, in order (k={k})"
            );
        }
    }
}

/// A failed step leaves the version unapplied; a retry (with the failure removed)
/// resumes and succeeds, applying every step exactly once.
#[test]
fn failed_step_leaves_version_unapplied_then_retry_succeeds() {
    let (_home, layout) = temp_store();
    let name = "t01_04::failstep::b";

    // Fail step b (seq 1).
    {
        let mut conn = raw_conn(&layout.state_db());
        seed_markers(&conn);
        arm(name);
        let err = run(&mut conn, FAILSTEP_SET, &layout.migration_lock(), 1000).expect_err("fail");
        assert!(matches!(err, MigrationError::Sqlite(_)));

        // Only step a committed; version not applied.
        assert!(!migration_applied(&conn, 1));
        assert_eq!(progress_seqs(&conn, 1), vec![0]);
        assert_eq!(marker_labels(&conn), vec!["a"]);
    }

    // Retry after removing the failure.
    disarm(name);
    {
        let mut conn = raw_conn(&layout.state_db());
        let report = run(&mut conn, FAILSTEP_SET, &layout.migration_lock(), 2000).expect("retry");
        assert_eq!(report.applied, vec![1]);
        assert!(migration_applied(&conn, 1));
        assert!(progress_seqs(&conn, 1).is_empty());
        assert_eq!(
            marker_labels(&conn),
            vec!["a", "b", "c"],
            "each step applied exactly once after retry"
        );
    }
}

/// A destructive migration takes a `VACUUM INTO` backup before mutating; the
/// backup opens as an independent database with the pre-change schema and data.
#[test]
fn destructive_backup_has_pre_change_data() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());

    run(&mut conn, BACKUP_SET_V1, &layout.migration_lock(), 1000).expect("v1 seed");
    assert_eq!(data_values(&conn), vec![42], "seeded");

    let report = run(&mut conn, BACKUP_SET_V12, &layout.migration_lock(), 2000).expect("v2 wipe");
    assert_eq!(report.applied, vec![2]);
    assert!(data_values(&conn).is_empty(), "live data wiped by v2");

    // Backup exists at the documented path, opens, and holds pre-change data.
    let backup = layout.backups_dir().join("state-2-2000.sqlite");
    assert!(backup.is_file(), "backup at {}", backup.display());
    assert_eq!(
        backup_data_values(&backup),
        vec![42],
        "backup captured the pre-change state"
    );
    assert_eq!(count_backups(&layout), 1, "exactly one backup written");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "backup file is 0600");
    }
}

/// A crash after the backup checkpoint but before the destructive step resumes
/// without re-taking the backup: the backup is not overwritten with a
/// post-mutation snapshot and exactly one backup file exists.
#[test]
fn destructive_backup_idempotent_on_resume() {
    let (_home, layout) = temp_store();
    let name = "t01_04::backup_resume::wipe";

    // v1 seeds; v2 takes the backup, then the wipe step fails.
    {
        let mut conn = raw_conn(&layout.state_db());
        run(&mut conn, BACKUP_SET_V1, &layout.migration_lock(), 1000).expect("v1 seed");
        arm(name);
        let err = run(&mut conn, BACKUP_RESUME_SET, &layout.migration_lock(), 2000)
            .expect_err("wipe step fails after backup");
        assert!(matches!(err, MigrationError::Sqlite(_)));

        // Backup committed (seq 0); wipe rolled back → data intact; not finalized.
        assert_eq!(progress_seqs(&conn, 2), vec![0]);
        assert!(!migration_applied(&conn, 2));
        assert_eq!(data_values(&conn), vec![42], "wipe rolled back");
    }
    let backup = layout.backups_dir().join("state-2-2000.sqlite");
    assert_eq!(count_backups(&layout), 1, "one backup after the crash");
    assert_eq!(backup_data_values(&backup), vec![42], "pre-change backup");

    // Resume: backup skipped, wipe applied, finalized. No new backup.
    disarm(name);
    {
        let mut conn = raw_conn(&layout.state_db());
        let report =
            run(&mut conn, BACKUP_RESUME_SET, &layout.migration_lock(), 3000).expect("resume");
        assert_eq!(report.applied, vec![2]);
        assert!(data_values(&conn).is_empty(), "wipe applied on resume");
        assert!(migration_applied(&conn, 2));
        assert!(progress_seqs(&conn, 2).is_empty());
    }
    assert_eq!(
        count_backups(&layout),
        1,
        "backup was NOT re-taken on resume"
    );
    assert_eq!(
        backup_data_values(&backup),
        vec![42],
        "backup still holds the pre-change state after resume"
    );
}

/// A destructive, SQL-only migration (no Rust steps) backs up first, then applies
/// its DDL as a checkpointed unit and finalizes.
#[test]
fn destructive_sql_only_backs_up_then_applies_sql() {
    let (_home, layout) = temp_store();
    let mut conn = raw_conn(&layout.state_db());

    run(&mut conn, BACKUP_SET_V1, &layout.migration_lock(), 1000).expect("v1 seed");
    let report = run(
        &mut conn,
        DESTRUCTIVE_SQL_SET,
        &layout.migration_lock(),
        2000,
    )
    .expect("v2 ddl");
    assert_eq!(report.applied, vec![2]);

    // The new table exists, the migration is finalized, and a backup was taken.
    let extra_exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='extra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(extra_exists, 1, "SQL unit applied");
    assert!(migration_applied(&conn, 2));
    assert!(progress_seqs(&conn, 2).is_empty(), "progress cleared");

    let backup = layout.backups_dir().join("state-2-2000.sqlite");
    assert!(backup.is_file());
    // The backup predates the DDL, so it must NOT contain `extra`.
    let bconn = Connection::open(&backup).unwrap();
    let extra_in_backup: i64 = bconn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='extra'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(extra_in_backup, 0, "backup is pre-change (no `extra`)");
}

/// Resume from the exact durable state of a crash between the last unit and the
/// finalize commit: all unit progress rows present, no `schema_migrations` row.
/// The runner skips every unit and finalizes exactly once.
#[test]
fn resume_from_finalize_pending() {
    let (_home, layout) = temp_store();

    // Bootstrap the framework tables and seed the "finalize pending" state.
    {
        let mut conn = raw_conn(&layout.state_db());
        seed_markers(&conn);
        run(&mut conn, &[], &layout.migration_lock(), 1).expect("bootstrap");
        for (seq, label) in RESUME_LABELS.iter().enumerate() {
            conn.execute(
                "INSERT INTO migration_progress(version, seq, label, done_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                (1i64, seq as i64, *label, 10i64),
            )
            .expect("seed progress");
        }
    }

    // Resume: all units already checkpointed → skipped; only finalize runs.
    {
        let mut conn = raw_conn(&layout.state_db());
        let report =
            run(&mut conn, RESUME_SET, &layout.migration_lock(), 2000).expect("finalize resume");
        assert_eq!(report.applied, vec![1]);
    }

    {
        let conn = raw_conn(&layout.state_db());
        assert!(
            marker_labels(&conn).is_empty(),
            "no step re-ran: their units were already checkpointed"
        );
        assert!(migration_applied(&conn, 1));
        assert!(progress_seqs(&conn, 1).is_empty(), "progress cleared");
    }

    // A further open is a clean no-op.
    {
        let mut conn = raw_conn(&layout.state_db());
        let report = run(&mut conn, RESUME_SET, &layout.migration_lock(), 3000).expect("no-op");
        assert!(report.applied.is_empty());
    }
}

// ---- hard-kill (SIGABRT) end-to-end -----------------------------------------

/// A genuine process crash after the backup checkpoint durably commits, followed
/// by a resume in a new process.
///
/// The test re-executes itself as a child (guarded by an env var). The child
/// arms the runner's feature-gated `migrate:after_backup` seam to `abort()` and
/// runs the destructive migration; it dies with `SIGABRT` right after the backup
/// commits. The parent confirms the signal, then resumes to completion — the
/// backup is neither lost nor re-taken.
///
/// Gated on `unix` + `failpoints`: the `abort()` seam only exists with the
/// feature, and signal inspection is POSIX-specific. Run via
/// `cargo test -p local-rag-store --features failpoints`.
#[cfg(all(unix, feature = "failpoints"))]
#[test]
fn resumable_hard_kill_via_sigabrt() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    use local_rag_test_support::run_capturing;

    const CHILD_ENV: &str = "LOCAL_RAG_T0104_SIGABRT_CHILD";
    const NOW_MS: i64 = 5000;

    // Child mode: perform the destructive migration and abort after the backup.
    if let Ok(root) = std::env::var(CHILD_ENV) {
        let layout = StoreLayout::new(std::path::PathBuf::from(root));
        let mut conn = raw_conn(&layout.state_db());
        run(&mut conn, BACKUP_SET_V1, &layout.migration_lock(), NOW_MS).expect("child v1");

        let fp = local_rag_test_support::failpoint::global();
        fp.register("migrate:after_backup");
        fp.arm("migrate:after_backup", Action::Abort)
            .expect("arm abort");

        // Expected to abort inside the runner right after the backup checkpoint.
        let _ = run(&mut conn, BACKUP_SET_V12, &layout.migration_lock(), NOW_MS);
        // Reaching here means the seam did not fire — fail loudly (not a signal).
        std::process::exit(97);
    }

    // Parent mode.
    let (_home, layout) = temp_store();

    let mut cmd = Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg("resumable_hard_kill_via_sigabrt")
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, layout.root());
    let outcome = run_capturing(cmd, "t01_04-sigabrt").expect("spawn child");

    assert_eq!(
        outcome.status.signal(),
        Some(6),
        "child must die with SIGABRT; status={:?} bundle={:?}\nstderr:\n{}",
        outcome.status,
        outcome.bundle,
        outcome.stderr_lossy()
    );

    // The backup was taken pre-mutation and survives the crash.
    let backup = layout
        .backups_dir()
        .join(format!("state-2-{NOW_MS}.sqlite"));
    assert!(backup.is_file(), "backup survived the hard kill");
    assert_eq!(backup_data_values(&backup), vec![42], "pre-change backup");
    assert_eq!(count_backups(&layout), 1);

    // Resume in this (fresh) process: backup skipped, wipe applied, finalized.
    let mut conn = raw_conn(&layout.state_db());
    let report = run(&mut conn, BACKUP_SET_V12, &layout.migration_lock(), NOW_MS).expect("resume");
    assert_eq!(report.applied, vec![2]);
    assert!(data_values(&conn).is_empty(), "wipe applied on resume");
    assert!(migration_applied(&conn, 2));
    assert!(progress_seqs(&conn, 2).is_empty());
    assert_eq!(count_backups(&layout), 1, "backup not re-taken on resume");
}
