//! T01-05 acceptance tests for the `cache.sqlite` open policy, store binding, and
//! recreation (spec 03 §1.4, §4, §4.4; 02 §5 L4b; 13 §3).
//!
//! All tests are deterministic: no network, no `$HOME` dependency (isolated
//! [`TempHome`]), and no wall-clock sleeps. Reopen-based tests fully drop the
//! prior [`CacheDb`] before reopening; the backpressure test gates the writer
//! thread with std channels and polls the blocked producer exactly once. The
//! hard-kill test uses a named failpoint + a real `SIGABRT` child, never a sleep.

use std::path::{Path, PathBuf};

use local_rag_core::hash::sha256_hex;
use local_rag_core::paths::StoreLayout;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{CACHE_SCHEMA_VERSION, CacheDb, CacheOpenOutcome, StateDb};
use local_rag_test_support::TempHome;

const UUID_A: &str = "11111111-1111-7111-8111-111111111111";
const UUID_B: &str = "22222222-2222-7222-8222-222222222222";

// ---- helpers ----------------------------------------------------------------

/// A temp store with an ensured tree; returns the home (kept alive for cleanup)
/// and its [`StoreLayout`]. The `cache.sqlite` file is created lazily by
/// [`CacheDb::open`], not by `ensure`.
fn temp_store() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

/// Open the cache at the layout's canonical path, bound to `uuid`.
fn open_cache(layout: &StoreLayout, uuid: &str, capacity: usize) -> CacheDb {
    CacheDb::open_with_capacity(layout.cache_db(), uuid, capacity).expect("open cache.sqlite")
}

/// Read a single `cache_meta` value through a read-only connection.
fn cache_meta(db: &CacheDb, key: &str) -> Option<String> {
    let conn = db.open_read().expect("open read-only cache");
    conn.query_row(
        "SELECT value FROM cache_meta WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Create a small `probe` table and insert one row through the write queue.
async fn write_probe(db: &CacheDb) {
    db.writer()
        .transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE probe (id INTEGER PRIMARY KEY);
                 INSERT INTO probe (id) VALUES (1);",
            )
        })
        .await
        .expect("write probe");
}

/// Whether the `probe` table exists and holds its row, via a read-only connection.
fn probe_present(db: &CacheDb) -> bool {
    let conn = db.open_read().expect("open read-only cache");
    conn.query_row("SELECT COUNT(*) FROM probe", [], |r| r.get::<_, i64>(0))
        .map(|n| n == 1)
        .unwrap_or(false)
}

// ---- tests ------------------------------------------------------------------

/// The cache connection pragmas are applied (spec 03 §4): WAL, `foreign_keys=OFF`,
/// `synchronous=NORMAL`, `busy_timeout=5000` — differing from state on FK and
/// synchronous.
#[tokio::test]
async fn cache_pragmas_are_applied() {
    let (_home, layout) = temp_store();
    let db = open_cache(&layout, UUID_A, 8);

    let (journal_mode, foreign_keys, synchronous, busy_timeout) = db
        .writer()
        .transaction(|tx| {
            let journal_mode: String = tx.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let foreign_keys: i64 = tx.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            let synchronous: i64 = tx.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            let busy_timeout: i64 = tx.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
            Ok((journal_mode, foreign_keys, synchronous, busy_timeout))
        })
        .await
        .expect("read pragmas");

    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    assert_eq!(foreign_keys, 0, "foreign_keys=OFF");
    assert_eq!(synchronous, 1, "synchronous=NORMAL (1)");
    assert_eq!(busy_timeout, 5000);
}

/// A first open creates and binds the cache; a matching reopen (same store UUID,
/// same schema version) reuses it untouched — rows and binding are preserved.
#[tokio::test]
async fn matching_reopen_preserves_rows() {
    let (_home, layout) = temp_store();

    let created_at = {
        let db = open_cache(&layout, UUID_A, 8);
        assert_eq!(
            db.outcome(),
            CacheOpenOutcome::Created,
            "first open creates"
        );
        write_probe(&db).await;
        assert_eq!(
            cache_meta(&db, "store_instance_uuid").as_deref(),
            Some(UUID_A)
        );
        cache_meta(&db, "created_at").expect("created_at seeded")
        // db dropped here → writer thread closes the connection
    };

    let db = open_cache(&layout, UUID_A, 8);
    assert_eq!(
        db.outcome(),
        CacheOpenOutcome::Reused,
        "matching reopen reuses"
    );
    assert!(
        probe_present(&db),
        "rows preserved across a matching reopen"
    );
    assert_eq!(
        cache_meta(&db, "created_at").as_deref(),
        Some(created_at.as_str()),
        "binding was not reseeded (created_at unchanged)"
    );
}

/// A reopen with a different store UUID drops and rebuilds the cache: prior rows
/// are gone and the binding now names the new store (spec 03 §4.4 step 1).
#[tokio::test]
async fn uuid_mismatch_rebuilds() {
    let (_home, layout) = temp_store();

    {
        let db = open_cache(&layout, UUID_A, 8);
        write_probe(&db).await;
    }

    let db = open_cache(&layout, UUID_B, 8);
    assert_eq!(
        db.outcome(),
        CacheOpenOutcome::Recreated,
        "UUID mismatch rebuilds"
    );
    assert!(!probe_present(&db), "rebuilt cache is empty");
    assert_eq!(
        cache_meta(&db, "store_instance_uuid").as_deref(),
        Some(UUID_B),
        "rebuilt cache is bound to the new store"
    );
    assert_eq!(
        cache_meta(&db, "cache_schema_version").as_deref(),
        Some(CACHE_SCHEMA_VERSION.to_string().as_str())
    );
}

/// A reopen against an unsupported `cache_schema_version` drops and rebuilds the
/// cache (spec 03 §4.4 step 2; 13 §3 — the cache is never migrated).
#[tokio::test]
async fn schema_version_mismatch_rebuilds() {
    let (_home, layout) = temp_store();
    let path = layout.cache_db();

    {
        let db = open_cache(&layout, UUID_A, 8);
        write_probe(&db).await;
    }

    // Poke an unsupported schema version straight into the file (a future binary
    // that wrote a newer cache layout).
    {
        let conn = Connection::open(&path).expect("raw open cache");
        conn.execute(
            "UPDATE cache_meta SET value = '999' WHERE key = 'cache_schema_version'",
            [],
        )
        .expect("bump schema version");
    }

    let db = open_cache(&layout, UUID_A, 8);
    assert_eq!(
        db.outcome(),
        CacheOpenOutcome::Recreated,
        "bad version rebuilds"
    );
    assert!(!probe_present(&db));
    assert_eq!(
        cache_meta(&db, "cache_schema_version").as_deref(),
        Some(CACHE_SCHEMA_VERSION.to_string().as_str()),
        "rebuilt cache carries the supported version"
    );
}

/// A corrupt cache file (a non-database file sitting at the cache path) is dropped
/// and rebuilt into a clean, usable, correctly-bound cache (spec 03 §4.4 —
/// "rebuild on doubt").
///
/// The garbage is written straight to the path with no live connection open, so
/// the next open genuinely sees a non-database file (a live WAL connection would
/// otherwise keep serving valid pages and mask the corruption).
#[tokio::test]
async fn corrupt_cache_yields_clean_cache() {
    let (_home, layout) = temp_store();
    let path = layout.cache_db();

    std::fs::write(&path, b"this is not a sqlite database, not at all").expect("corrupt cache");

    let db = open_cache(&layout, UUID_A, 8);
    assert_eq!(
        db.outcome(),
        CacheOpenOutcome::Recreated,
        "corruption rebuilds"
    );
    assert_eq!(
        cache_meta(&db, "store_instance_uuid").as_deref(),
        Some(UUID_A)
    );

    // The rebuilt cache is usable.
    write_probe(&db).await;
    assert!(probe_present(&db), "rebuilt cache accepts writes");
}

/// Rebuilding the cache never touches `state.sqlite`: its file bytes are unchanged
/// and its rows remain readable (spec 03 §1.4/§4.4 — cache loss loses nothing).
#[tokio::test]
async fn state_untouched_on_cache_rebuild() {
    let (_home, layout) = temp_store();

    // Open state and commit a probe row into `store_settings` (bootstrapped DDL).
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    state
        .writer()
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO store_settings (key, value) VALUES ('probe', 'kept')",
                [],
            )
            .map(|_| ())
        })
        .await
        .expect("seed state row");

    let state_hash_before = sha256_hex(&std::fs::read(layout.state_db()).expect("read state file"));

    // Build a cache bound to A, then force a rebuild by reopening with B.
    {
        let db = open_cache(&layout, UUID_A, 8);
        write_probe(&db).await;
    }
    let db = open_cache(&layout, UUID_B, 8);
    assert_eq!(db.outcome(), CacheOpenOutcome::Recreated);

    let state_hash_after = sha256_hex(&std::fs::read(layout.state_db()).expect("read state file"));
    assert_eq!(
        state_hash_before, state_hash_after,
        "cache rebuild left state.sqlite byte-identical"
    );

    let kept: String = state
        .open_read()
        .expect("open state read")
        .query_row(
            "SELECT value FROM store_settings WHERE key = 'probe'",
            [],
            |r| r.get(0),
        )
        .expect("state row still present");
    assert_eq!(kept, "kept", "state data intact after cache rebuild");
}

/// A full cache write queue makes a producer wait on backpressure; cancelling that
/// producer (dropping its future) frees the slot cleanly, runs no partial write,
/// and leaves the cache consistent and usable (spec 02 §5 L4b).
///
/// Determinism mirrors the state backpressure test: std-channel gating, a
/// [`spawn_blocking`] wait for "A has started", and polling C exactly once.
///
/// [`spawn_blocking`]: tokio::task::spawn_blocking
#[tokio::test]
async fn cache_writer_backpressure_cancels_cleanly() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let (_home, layout) = temp_store();
    let db = open_cache(&layout, UUID_A, 1); // single slot → easy to saturate
    db.writer()
        .transaction(|tx| tx.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);"))
        .await
        .expect("create schema");

    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();

    // Job A occupies the writer thread until the gate opens.
    let writer_a = db.writer().clone();
    let job_a = tokio::spawn(async move {
        writer_a
            .transaction(move |tx| {
                tx.execute("INSERT INTO t (id) VALUES (1)", [])?;
                started_tx.send(()).ok();
                gate_rx.recv().ok();
                Ok::<(), local_rag_store::rusqlite::Error>(())
            })
            .await
    });

    // Wait (off the runtime) until A is executing; its queue slot is free again.
    tokio::task::spawn_blocking(move || started_rx.recv().ok())
        .await
        .expect("join started-wait");

    // Job B fills the single slot but cannot run (writer busy with A).
    let writer_b = db.writer().clone();
    let job_b = tokio::spawn(async move {
        writer_b
            .transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (2)", []).map(|_| ()))
            .await
    });
    while db.writer().available_slots() > 0 {
        tokio::task::yield_now().await;
    }

    // Job C must block on backpressure — its very first `send` poll is Pending.
    let writer_c = db.writer().clone();
    let mut job_c = Box::pin(
        writer_c.transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (3)", []).map(|_| ())),
    );
    let c_pending = poll_fn(|cx| Poll::Ready(job_c.as_mut().poll(cx).is_pending())).await;
    assert!(c_pending, "C must wait while the queue is full");

    // Cancel C — its send never occupied a slot.
    drop(job_c);

    gate_tx.send(()).ok();
    job_a.await.expect("join A").expect("A committed");
    job_b.await.expect("join B").expect("B committed");

    let ids: Vec<i64> = {
        let conn = db.open_read().expect("read cache");
        let mut stmt = conn
            .prepare("SELECT id FROM t ORDER BY id")
            .expect("prepare");
        stmt.query_map([], |r| r.get::<_, i64>(0))
            .expect("query")
            .collect::<local_rag_store::rusqlite::Result<Vec<_>>>()
            .expect("collect")
    };
    assert_eq!(ids, vec![1, 2], "cancelled C left no row");

    db.writer()
        .transaction(|tx| tx.execute("INSERT INTO t (id) VALUES (4)", []).map(|_| ()))
        .await
        .expect("post-cancel write");
}

/// An interrupted rebuild (modelled by deleting the cache files) converges: the
/// next open rebuilds a valid bound cache, and a further open reuses it. This is
/// the required "re-run" check for a state-changing SQLite operation.
#[tokio::test]
async fn recreate_is_idempotent_on_retry() {
    let (_home, layout) = temp_store();
    let path = layout.cache_db();

    {
        let db = open_cache(&layout, UUID_A, 8);
        write_probe(&db).await;
    }

    // Model a crash mid-rebuild: the cache files are gone.
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(append(&path, suffix));
    }

    // First reopen rebuilds a fresh bound cache; a second reopen reuses it.
    {
        let db = open_cache(&layout, UUID_A, 8);
        assert_eq!(
            db.outcome(),
            CacheOpenOutcome::Created,
            "rebuild after loss"
        );
        assert_eq!(
            cache_meta(&db, "store_instance_uuid").as_deref(),
            Some(UUID_A)
        );
    }
    let db = open_cache(&layout, UUID_A, 8);
    assert_eq!(
        db.outcome(),
        CacheOpenOutcome::Reused,
        "second open converges"
    );
    assert_eq!(
        cache_meta(&db, "store_instance_uuid").as_deref(),
        Some(UUID_A)
    );
}

/// The cross-database rule (spec 03 §1.4 `[FIXED]`): the storage crate must not
/// contain a *writable* cross-DB `ATTACH`. This source lint flags any `ATTACH`
/// occurrence in real code (not comments) that is not annotated as read-only, so
/// a future writable-ATTACH path cannot land silently. Read-only `ATTACH` is
/// permitted and must carry a `// cross-db: read-only` marker.
#[test]
fn no_writable_cross_db_attach() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs_files(&src, &mut files);
    assert!(!files.is_empty(), "found no source files under {src:?}");

    let mut violations = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read source file");
        for (i, line) in text.lines().enumerate() {
            if !line.to_ascii_lowercase().contains("attach") {
                continue;
            }
            let is_comment = line.trim_start().starts_with("//");
            let allowed = line.contains("cross-db: read-only");
            if !is_comment && !allowed {
                violations.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "writable cross-DB ATTACH is forbidden (spec 03 §1.4); found:\n{}",
        violations.join("\n")
    );
}

// ---- hard-kill (SIGABRT) end-to-end -----------------------------------------

/// A genuine process crash mid-rebuild — right after the stale files are removed
/// and a fresh (still unbound) cache file is created, but before `cache_meta` is
/// seeded — leaves an unbound cache. A resume in a new process detects it and
/// rebuilds a valid bound cache.
///
/// The test re-executes itself as a child (guarded by an env var). The child
/// binds a cache to A, then reopens with B (a rebuild) and aborts (`SIGABRT`) at
/// the feature-gated `cache:after_delete` seam. The parent confirms the signal,
/// then opens with B and gets a clean cache bound to B.
///
/// Gated on `unix` + `failpoints`. Run via
/// `cargo test -p local-rag-store --features failpoints`.
#[cfg(all(unix, feature = "failpoints"))]
#[test]
fn cache_recreate_hard_kill_resumes() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    use local_rag_test_support::{Action, run_capturing};

    const CHILD_ENV: &str = "LOCAL_RAG_T0105_SIGABRT_CHILD";

    // Child mode: bind to A, then abort mid-rebuild while reopening with B.
    if let Ok(root) = std::env::var(CHILD_ENV) {
        let layout = StoreLayout::new(PathBuf::from(root));
        let db_a = CacheDb::open(layout.cache_db(), UUID_A).expect("child bind A");
        drop(db_a);

        let fp = local_rag_test_support::failpoint::global();
        fp.register("cache:after_delete");
        fp.arm("cache:after_delete", Action::Abort)
            .expect("arm abort");

        // Expected to abort inside open_and_bind right after the fresh file is
        // created but before the binding is seeded.
        let _ = CacheDb::open(layout.cache_db(), UUID_B);
        std::process::exit(97); // reached only if the seam did not fire
    }

    // Parent mode.
    let (_home, layout) = temp_store();

    let mut cmd = Command::new(std::env::current_exe().expect("current exe"));
    cmd.arg("cache_recreate_hard_kill_resumes")
        .arg("--exact")
        .arg("--nocapture")
        .env(CHILD_ENV, layout.root());
    let outcome = run_capturing(cmd, "t01_05-sigabrt").expect("spawn child");

    assert_eq!(
        outcome.status.signal(),
        Some(6),
        "child must die with SIGABRT; status={:?} bundle={:?}\nstderr:\n{}",
        outcome.status,
        outcome.bundle,
        outcome.stderr_lossy()
    );

    // Resume in this (fresh) process: the unbound leftover is detected and rebuilt.
    let db = CacheDb::open(layout.cache_db(), UUID_B).expect("parent resume");
    assert_eq!(
        db.outcome(),
        CacheOpenOutcome::Recreated,
        "the crashed-mid-rebuild cache is rebuilt, not trusted"
    );
    assert_eq!(
        cache_meta(&db, "store_instance_uuid").as_deref(),
        Some(UUID_B)
    );
}

// ---- local helpers ----------------------------------------------------------

/// Append a suffix to a path's file name (`cache.sqlite` → `cache.sqlite-wal`).
fn append(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Collect every `.rs` file under `dir`, recursively.
fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read_dir src") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
