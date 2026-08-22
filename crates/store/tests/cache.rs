//! T01-05 acceptance tests for the `cache.sqlite` open policy, store binding, and
//! recreation (spec 03 §1.4, §4, §4.4; 02 §5 L4b; 13 §3).
//!
//! All tests are deterministic: no network, no `$HOME` dependency (isolated
//! [`TempHome`]), and no wall-clock sleeps. Reopen-based tests close the prior
//! [`CacheDb`] with [`CacheDb::close`] — a *waiting* close — before reopening or
//! unlinking the path: a plain drop only closes the write queue, letting the
//! writer thread tear its connection down concurrently, and SQLite opens
//! `-wal`/`-shm` **by name**, so that teardown can land on the files the next
//! instance just created (D-009); the backpressure test gates the writer
//! thread with std channels and polls the blocked producer exactly once. The
//! hard-kill test uses a named failpoint + a real `SIGABRT` child, never a sleep.

use std::path::{Path, PathBuf};

use local_rag_core::hash::sha256_hex;
use local_rag_core::paths::StoreLayout;
use local_rag_store::rusqlite::{Connection, Error, ErrorCode};
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

/// The busy/locked error family a fresh read connection can hit transiently right
/// after a rebuild — notably `SQLITE_BUSY_SNAPSHOT` (primary code `SQLITE_BUSY`),
/// which the connection's `busy_timeout` does **not** wait out. Retrying on a fresh
/// connection takes a new WAL snapshot and clears it.
fn is_transient(e: &Error) -> bool {
    matches!(
        e,
        Error::SqliteFailure(err, _)
            if matches!(err.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// A "no such table" error — a *legitimate absence* for [`probe_present`] after a
/// rebuild wiped the table, distinct from a transient lock or an unexpected fault.
fn is_no_such_table(e: &Error) -> bool {
    matches!(e, Error::SqliteFailure(_, Some(msg)) if msg.contains("no such table"))
}

/// Run a single-row read against a fresh read-only cache connection, returning
/// `None` **only** for a genuinely absent row (`QueryReturnedNoRows`).
///
/// Transient busy/locked contention is retried on a fresh connection; any other
/// error panics loudly rather than masquerading as an absent row. This replaces the
/// earlier `.ok()`/`unwrap_or(false)` that conflated a transient lock with absence
/// and made these helpers non-deterministic under parallel load (D-003). No
/// wall-clock sleep: each connection's `busy_timeout` waits out ordinary contention
/// internally, and a fresh snapshot clears `BUSY_SNAPSHOT` on the next attempt.
fn read_optional<T>(db: &CacheDb, query: impl Fn(&Connection) -> rusqlite::Result<T>) -> Option<T> {
    const ATTEMPTS: usize = 16;
    let mut last: Option<Error> = None;
    for _ in 0..ATTEMPTS {
        let conn = db.open_read().expect("open read-only cache");
        match query(&conn) {
            Ok(value) => return Some(value),
            Err(Error::QueryReturnedNoRows) => return None,
            Err(e) if is_transient(&e) => last = Some(e),
            Err(e) => panic!("cache read failed (not a transient lock): {e}"),
        }
    }
    panic!("cache read stayed busy after {ATTEMPTS} fresh attempts: {last:?}");
}

/// Read a single `cache_meta` value through a read-only connection. `None` means
/// the key is genuinely absent, never a masked transient error (D-003).
fn cache_meta(db: &CacheDb, key: &str) -> Option<String> {
    read_optional(db, |conn| {
        conn.query_row(
            "SELECT value FROM cache_meta WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        )
    })
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
///
/// A wiped table after a rebuild (`no such table`) is a legitimate `false`;
/// transient busy/locked contention is retried; any other error panics (never
/// masked as `false`, the pre-D-003 `unwrap_or(false)` behaviour).
fn probe_present(db: &CacheDb) -> bool {
    const ATTEMPTS: usize = 16;
    let mut last: Option<Error> = None;
    for _ in 0..ATTEMPTS {
        let conn = db.open_read().expect("open read-only cache");
        match conn.query_row("SELECT COUNT(*) FROM probe", [], |r| r.get::<_, i64>(0)) {
            Ok(n) => return n == 1,
            Err(e) if is_no_such_table(&e) => return false,
            Err(e) if is_transient(&e) => last = Some(e),
            Err(e) => panic!("probe read failed (not transient / no-such-table): {e}"),
        }
    }
    panic!("probe read stayed busy after {ATTEMPTS} fresh attempts: {last:?}");
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

/// `D-092`: the cache writer, like state's, owns the write lock from `BEGIN`.
///
/// Same defect, same queue shape, so the same regression guards it — see
/// `crates/store/tests/state.rs`'s twin for the mechanism SQLite documents
/// (no busy handler on a read-lock → write-lock promotion, so `busy_timeout`
/// never gets to do its job). Deterministic by construction: the probe runs
/// inside a transaction that has touched nothing, where a `DEFERRED` `BEGIN`
/// would still be holding no lock at all.
#[tokio::test]
async fn the_cache_writer_holds_the_write_lock_from_begin_not_from_its_first_write() {
    use std::time::Duration;

    let (_home, layout) = temp_store();
    let db = open_cache(&layout, UUID_A, 8);
    let probe_path = layout.cache_db();

    let refused = db
        .writer()
        .transaction(move |_tx| {
            let other = Connection::open(&probe_path)?;
            other.busy_timeout(Duration::ZERO)?;
            match other.execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => Ok(None),
                Err(Error::SqliteFailure(e, _)) => Ok(Some((e.code, e.extended_code))),
                Err(e) => Err(e),
            }
        })
        .await
        .expect("the probe itself must run");

    assert_eq!(
        refused,
        Some((ErrorCode::DatabaseBusy, 5)),
        "a foreign writer must find the lock already held (D-092); `None` means the writer began \
         DEFERRED and left the door open until its first write"
    );

    db.close();
}

/// D-027 (spec 12 §6 `[FIXED]` "files/segments 0600"): `cache.sqlite` itself is
/// created at `0600`, not left at the process umask's default. Mirrors
/// `crates/store/tests/state.rs`'s identical regression for `state.sqlite`.
#[cfg(unix)]
#[tokio::test]
async fn cache_db_open_creates_and_reasserts_cache_sqlite_at_0600() {
    use std::os::unix::fs::PermissionsExt;

    let (_home, layout) = temp_store();
    let db = open_cache(&layout, UUID_A, 8);
    let mode = std::fs::metadata(layout.cache_db())
        .expect("stat cache.sqlite")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "freshly created cache.sqlite is 0600");
    db.close();

    std::fs::set_permissions(layout.cache_db(), std::fs::Permissions::from_mode(0o644))
        .expect("widen mode");
    let db2 = open_cache(&layout, UUID_A, 8);
    let mode = std::fs::metadata(layout.cache_db())
        .expect("stat cache.sqlite")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "reopening re-asserts 0600");
    db2.close();
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
        db.close();
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
        db.close();
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
        db.close();
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
        db.close();
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
        db.close();
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
    db.close();
}

/// D-009 regression: [`CacheDb::close`] must *wait* for the writer thread, so a
/// caller may unlink and recreate the path immediately afterwards.
///
/// Without that barrier the previous instance's detached writer is still closing
/// its connection while the next instance creates a new database at the same
/// path; because SQLite opens `-wal`/`-shm` **by name**, the old teardown lands
/// on the *new* instance's sidecars, leaving an empty database whose reader gets
/// `SQLITE_IOERR_SHORT_READ`. Measured before the fix: 6 of 16 concurrent
/// processes failed; after it, 0 of 16.
///
/// The loop is the whole point — one iteration would pass even with the race, so
/// this repeats the unlink/recreate cycle and asserts every round is readable
/// through an independent read-only connection.
#[tokio::test]
async fn close_lets_the_path_be_unlinked_and_recreated_immediately() {
    let (_home, layout) = temp_store();
    let path = layout.cache_db();

    for round in 0..8 {
        let db = open_cache(&layout, UUID_A, 8);
        write_probe(&db).await;
        assert!(probe_present(&db), "round {round}: probe written");
        // The barrier under test: after this returns, nothing of this instance
        // may touch the files any more.
        db.close();

        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(append(&path, suffix));
        }

        let rebuilt = open_cache(&layout, UUID_A, 8);
        assert_eq!(
            rebuilt.outcome(),
            CacheOpenOutcome::Created,
            "round {round}: unlinked cache is rebuilt"
        );
        assert_eq!(
            cache_meta(&rebuilt, "store_instance_uuid").as_deref(),
            Some(UUID_A),
            "round {round}: the rebuilt cache is readable through a fresh \
             read-only connection"
        );
        assert!(
            !probe_present(&rebuilt),
            "round {round}: the rebuilt cache starts empty"
        );
        rebuilt.close();

        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(append(&path, suffix));
        }
    }
}

/// D-003 regression: the read-helper error classifiers discriminate correctly, so
/// a transient lock can never be masked as an absent row (the pre-D-003 `.ok()` /
/// `unwrap_or(false)` bug that made `recreate_is_idempotent_on_retry` flaky under
/// parallel load). `SQLITE_BUSY_SNAPSHOT` — the specific transient the connection's
/// `busy_timeout` does not wait out — must classify as transient (retryable), not
/// as absence, and a wiped table must be absence, not a masked fault.
#[test]
fn read_helper_classifiers_discriminate() {
    use local_rag_store::rusqlite::ffi;

    // The busy/locked family (incl. BUSY_SNAPSHOT, primary code SQLITE_BUSY) is
    // transient → retried on a fresh connection, never returned as absence.
    for code in [
        ffi::SQLITE_BUSY,
        ffi::SQLITE_BUSY_SNAPSHOT,
        ffi::SQLITE_LOCKED,
    ] {
        let e = Error::SqliteFailure(ffi::Error::new(code), None);
        assert!(is_transient(&e), "extended code {code} must be transient");
        assert!(!is_no_such_table(&e), "a busy error is not a missing table");
    }

    // A genuinely absent row is signalled by QueryReturnedNoRows, not a busy error.
    assert!(!is_transient(&Error::QueryReturnedNoRows));

    // "no such table" is a legitimate absence for `probe_present`, not transient.
    let missing = Error::SqliteFailure(
        ffi::Error::new(ffi::SQLITE_ERROR),
        Some("no such table: probe".to_string()),
    );
    assert!(is_no_such_table(&missing));
    assert!(!is_transient(&missing));

    // A generic fault is neither → the helpers panic loudly instead of masking it
    // as absence.
    let generic = Error::SqliteFailure(
        ffi::Error::new(ffi::SQLITE_ERROR),
        Some("near \"SELCT\": syntax error".to_string()),
    );
    assert!(!is_transient(&generic));
    assert!(!is_no_such_table(&generic));
}

/// D-003 regression (behavioural): the read helpers return a real `None`/`false`
/// only for genuine absence, and the present value otherwise — on a real cache, so
/// the query/visibility path is exercised end-to-end.
#[tokio::test]
async fn read_helpers_distinguish_present_from_absent() {
    let (_home, layout) = temp_store();
    let db = open_cache(&layout, UUID_A, 8);

    // A seeded key resolves; a genuinely absent key is a real None (not a masked
    // transient error).
    assert_eq!(
        cache_meta(&db, "store_instance_uuid").as_deref(),
        Some(UUID_A)
    );
    assert_eq!(cache_meta(&db, "definitely_absent_key"), None);

    // `probe_present`: no table yet → false (a legitimate absence, not a panic);
    // after a write → true.
    assert!(!probe_present(&db), "no probe table before any write");
    write_probe(&db).await;
    assert!(probe_present(&db), "probe row present after write");
}

/// Whether `line` carries the SQL `ATTACH` keyword.
///
/// SQL in this crate is written with uppercase keywords by strict convention
/// (every `CREATE`/`INSERT`/`PRAGMA`/`VACUUM`/… is uppercase), so the uppercase
/// token is the SQL form. Keying on it deliberately does **not** match the Rust
/// identifier `attach` / `AttachError` / `Reattachable` — the spec-named
/// `repo attach` worktree operation (04 §7, T02-04), which is not SQL. Before
/// D-002 the scan lowercased the line and matched the substring `attach`, which
/// false-positived on that operation once it landed; the invariant enforced is
/// unchanged (no uppercase SQL `ATTACH` can slip in), only the identifier
/// collision is removed.
fn contains_sql_attach(line: &str) -> bool {
    line.contains("ATTACH")
}

/// The cross-database rule (spec 03 §1.4 `[FIXED]`): the storage crate must not
/// contain a *writable* cross-DB `ATTACH`. This source lint flags any SQL
/// `ATTACH` occurrence in real code (not comments) that is not annotated as
/// read-only, so a future writable-ATTACH path cannot land silently. Read-only
/// `ATTACH` is permitted and must carry a `// cross-db: read-only` marker.
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
            if !contains_sql_attach(line) {
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

/// D-002 regression: the source lint keys on the SQL `ATTACH` keyword, not on the
/// Rust `attach` identifier / `AttachError` type / `Reattachable` (the spec-named
/// `repo attach` operation, 04 §7). Uppercase SQL is still detected; the Rust
/// identifier is not.
#[test]
fn source_lint_targets_sql_attach_not_the_rust_identifier() {
    // Positive: real SQL keyword (uppercase by crate convention) is detected.
    assert!(contains_sql_attach(
        r#"tx.execute("ATTACH DATABASE 'x' AS y", [])"#
    ));
    assert!(contains_sql_attach("  ATTACH 'file' AS aux"));
    // Negative: the Rust `attach` operation and its types are not SQL.
    assert!(!contains_sql_attach("pub fn attach("));
    assert!(!contains_sql_attach(
        "        return Ok(Err(AttachError::UnknownWorktree));"
    ));
    assert!(!contains_sql_attach(
        "    NotReattachable(IllegalWorktreeTransition),"
    ));
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
