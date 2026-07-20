//! T03-04 acceptance tests: `normalized_text_cache` regeneration and the
//! `last_used_at` batching seam (spec 03 §2.3, §3, §4.2, §4.4; 06 §4).
//!
//! Pure normalization/identity golden cases live in the `code::normalize` module
//! unit tests and the seam dedup logic in the `cache::text` unit tests; these
//! exercise the DB-facing paths end to end — deriving from a stored `source_blob`,
//! caching through [`CacheWriter::transaction`](local_rag_store::CacheWriter),
//! reading through [`CacheDb::open_read`], detecting a corrupt row, regenerating,
//! and flushing batched `last_used_at` updates.
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, ids minted
//! from [`uuidv7_from`] with fixed entropy, no network, no `$HOME` dependency,
//! no wall-clock sleeps. Cache reads use a D-003-style fresh-connection retry so
//! parallel workspace load cannot flake them.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::rusqlite;
use local_rag_store::rusqlite::{Connection, Error, ErrorCode};
use local_rag_store::{
    BatchingLastUsed, BlobOutcome, CACHE_SCHEMA_VERSION, CacheDb, DerivedContentBlob, LastUsedSink,
    NormalizedTextRow, StateDb, create_or_reuse_content_blob, create_or_reuse_file_revision,
    delete_normalized_text, derive_content_blob, flush_last_used, get_normalized_text,
    insert_normalized_text, prepare_source, source_bytes, verify_cached_text,
};
use local_rag_test_support::TempHome;

const STORE_UUID: &str = "11111111-1111-7111-8111-111111111111";
const LANG: &str = "rust";

// ---- helpers ----------------------------------------------------------------

/// A temp store with an ensured tree plus opened `state.sqlite` and `cache.sqlite`
/// (the latter bound to [`STORE_UUID`]). The home is returned to keep it alive.
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

/// The busy/locked family a fresh cache read can hit transiently after a rebuild
/// (notably `SQLITE_BUSY_SNAPSHOT`, which `busy_timeout` cannot wait out). Mirrors
/// the D-003 classifier in `tests/cache.rs`.
fn is_transient(e: &Error) -> bool {
    matches!(
        e,
        Error::SqliteFailure(err, _)
            if matches!(err.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Read a `normalized_text_cache` row, retrying transient contention on a fresh
/// read-only connection. `None` is a genuine absence; a missing *table* (which
/// would surface as a non-transient error) panics rather than masquerading as
/// absent (D-003). No wall-clock sleep.
fn get_row(cache: &CacheDb, blob_id: &str) -> Option<NormalizedTextRow> {
    const ATTEMPTS: usize = 16;
    let mut last: Option<Error> = None;
    for _ in 0..ATTEMPTS {
        let conn = cache.open_read().expect("open read-only cache");
        match get_normalized_text(&conn, blob_id) {
            Ok(row) => return row,
            Err(e) if is_transient(&e) => last = Some(e),
            Err(e) => panic!("normalized_text read failed (not transient): {e}"),
        }
    }
    panic!("normalized_text read stayed busy after {ATTEMPTS} attempts: {last:?}");
}

/// Cache a derived blob's normalized text through the write queue.
async fn put(cache: &CacheDb, derived: &DerivedContentBlob, now_ms: i64) {
    let (blob_id, text, size) = (
        derived.blob_id.clone(),
        derived.normalized_text.clone(),
        derived.byte_size,
    );
    cache
        .writer()
        .transaction(move |tx| insert_normalized_text(tx, &blob_id, &text, size, now_ms))
        .await
        .expect("insert normalized text");
}

/// Ingest raw bytes into `state.sqlite` as a `file_revision`, returning its id.
async fn ingest_source(state: &StateDb, raw: &[u8], fingerprint: &str, seed: u8) -> String {
    let prepared = prepare_source(raw);
    let (fp, id) = (fingerprint.to_string(), uuid(seed));
    let outcome = state
        .writer()
        .transaction(move |tx| create_or_reuse_file_revision(tx, &prepared, &fp, &id, 1000))
        .await
        .expect("ingest source");
    outcome.id().to_string()
}

/// Re-derive a content blob from a stored `source_blob` (spec 06 §4 recompute path).
fn regenerate_from_source(state: &StateDb, file_revision_id: &str) -> DerivedContentBlob {
    let read = state.open_read().expect("read conn");
    let bytes = source_bytes(&read, file_revision_id)
        .expect("source_bytes")
        .expect("revision present");
    let text = std::str::from_utf8(&bytes).expect("stored source is valid UTF-8");
    derive_content_blob(LANG, text)
}

// ---- tests ------------------------------------------------------------------

/// The `normalized_text_cache` table is built at cache open (schema version 2):
/// a lookup on an empty cache returns `None` rather than erroring "no such table".
#[tokio::test]
async fn normalized_text_cache_table_exists_after_open() {
    assert_eq!(CACHE_SCHEMA_VERSION, 2, "T03-04 bumps the cache schema");
    let (_home, _state, cache) = open_both();
    // get_row panics if the table is missing; None here proves it exists & is empty.
    assert_eq!(get_row(&cache, "no-such-blob"), None);
}

/// Derive → cache → read reproduces the normalized text and its metadata.
#[tokio::test]
async fn roundtrip_derive_cache_read() {
    let (_home, _state, cache) = open_both();
    let derived = derive_content_blob(LANG, "\u{FEFF}fn  main() {}  \r\n");
    assert_eq!(derived.normalized_text, "fn  main() {}\n");

    put(&cache, &derived, 4242).await;

    let row = get_row(&cache, &derived.blob_id).expect("cached row present");
    assert_eq!(row.normalized_text, derived.normalized_text);
    assert_eq!(row.byte_size, derived.byte_size);
    assert_eq!(row.created_at, 4242);
    assert_eq!(row.last_used_at, 4242);
}

/// Deleting a cache row loses nothing: recomputing from the canonical `source_blob`
/// yields byte-identical normalized text and the same `blob_id` (spec 06 §4).
#[tokio::test]
async fn cache_delete_reconstructs_identically_from_source_blob() {
    let (_home, state, cache) = open_both();
    let raw = "\u{FEFF}fn demo() {\r\n    let x = 1;  \r\n}\r\n".as_bytes();
    let rev_id = ingest_source(&state, raw, "lang=rust;grammar=rs@1", 1).await;

    // Derive from the stored source and cache it; also materialize the identity.
    let first = regenerate_from_source(&state, &rev_id);
    put(&cache, &first, 1000).await;
    let outcome = {
        let (d, lang) = (first.clone(), LANG.to_string());
        state
            .writer()
            .transaction(move |tx| create_or_reuse_content_blob(tx, &d, &lang, 1000))
            .await
            .expect("materialize content_blob")
    };
    assert_eq!(outcome, BlobOutcome::Created(first.blob_id.clone()));
    assert_eq!(
        get_row(&cache, &first.blob_id)
            .expect("present")
            .normalized_text,
        first.normalized_text
    );

    // Evict the cache row.
    let blob_id = first.blob_id.clone();
    let removed = cache
        .writer()
        .transaction(move |tx| delete_normalized_text(tx, &blob_id))
        .await
        .expect("delete");
    assert!(removed, "the row was present");
    assert_eq!(get_row(&cache, &first.blob_id), None, "evicted");

    // Regenerate purely from the source_blob → identical identity and text.
    let second = regenerate_from_source(&state, &rev_id);
    assert_eq!(
        second.blob_id, first.blob_id,
        "identity reconstructs exactly"
    );
    assert_eq!(second.normalized_text, first.normalized_text);
    assert_eq!(second.byte_size, first.byte_size);

    put(&cache, &second, 2000).await;
    assert_eq!(
        get_row(&cache, &first.blob_id)
            .expect("re-cached")
            .normalized_text,
        first.normalized_text
    );
}

/// A corrupt cache row (text that no longer reproduces its `blob_id`) is detected
/// by recomputing the identity, then evicted and regenerated (spec 03 §4.4).
#[tokio::test]
async fn invalid_row_is_detected_and_regenerated() {
    let (_home, _state, cache) = open_both();
    let source_text = "fn valid() {}\n";
    let good = derive_content_blob(LANG, source_text);

    // Store TAMPERED text under the valid blob_id (a bit-rot / corruption stand-in).
    {
        let (blob_id, tampered) = (good.blob_id.clone(), "TAMPERED CONTENT".to_string());
        let size = tampered.len() as i64;
        cache
            .writer()
            .transaction(move |tx| insert_normalized_text(tx, &blob_id, &tampered, size, 1000))
            .await
            .expect("write tampered row");
    }

    let row = get_row(&cache, &good.blob_id).expect("row present");
    assert_eq!(row.normalized_text, "TAMPERED CONTENT");
    assert!(
        !verify_cached_text(&good.blob_id, LANG, &row.normalized_text),
        "corrupt text must fail identity verification"
    );

    // Evict + regenerate from source → now verifies.
    let blob_id = good.blob_id.clone();
    cache
        .writer()
        .transaction(move |tx| delete_normalized_text(tx, &blob_id))
        .await
        .expect("evict");
    let regenerated = derive_content_blob(LANG, source_text);
    assert_eq!(regenerated.blob_id, good.blob_id);
    put(&cache, &regenerated, 2000).await;

    let fixed = get_row(&cache, &good.blob_id).expect("re-cached");
    assert!(
        verify_cached_text(&good.blob_id, LANG, &fixed.normalized_text),
        "regenerated text verifies against its identity"
    );
}

/// The canonical `content_blob` row in `state.sqlite` carries identity + metadata
/// only — never the normalized text — and create-or-reuse is idempotent.
#[tokio::test]
async fn content_blob_state_row_has_no_text_and_reuse_is_idempotent() {
    let (_home, state, _cache) = open_both();
    let derived = derive_content_blob(LANG, "fn main() {}\n");

    let mut created = 0;
    let mut reused = 0;
    for _ in 0..3 {
        let (d, lang) = (derived.clone(), LANG.to_string());
        let outcome = state
            .writer()
            .transaction(move |tx| create_or_reuse_content_blob(tx, &d, &lang, 1000))
            .await
            .expect("create-or-reuse content_blob");
        match outcome {
            BlobOutcome::Created(id) => {
                created += 1;
                assert_eq!(id, derived.blob_id);
            }
            BlobOutcome::Reused(id) => {
                reused += 1;
                assert_eq!(id, derived.blob_id);
            }
        }
    }
    assert_eq!(created, 1, "exactly one create across retries");
    assert_eq!(reused, 2, "the two replays reuse");

    let read = state.open_read().expect("read conn");
    let n: i64 = read
        .query_row("SELECT count(*) FROM content_blob", [], |r| r.get(0))
        .expect("count");
    assert_eq!(n, 1, "idempotent: no duplicate content_blob rows");

    // The content_blob row must not carry a normalized_text column.
    let has_text = column_names(&read, "content_blob")
        .iter()
        .any(|c| c == "normalized_text");
    assert!(
        !has_text,
        "content_blob stores identity only; text lives in cache"
    );
}

/// The `last_used_at` batching seam: record (dedup to latest) → drain → flush as
/// one transaction updates `last_used_at` for existing rows, leaves `created_at`
/// untouched, and silently skips an evicted blob.
#[tokio::test]
async fn last_used_batching_seam_flushes() {
    let (_home, _state, cache) = open_both();
    let a = derive_content_blob(LANG, "fn a() {}\n");
    let b = derive_content_blob(LANG, "fn b() {}\n");
    put(&cache, &a, 1000).await;
    put(&cache, &b, 1000).await;

    let sink = BatchingLastUsed::new();
    sink.record_used(&a.blob_id, 5000);
    sink.record_used(&a.blob_id, 4000); // earlier — dedups to 5000
    sink.record_used(&b.blob_id, 6000);
    sink.record_used("evicted-blob", 7000); // not in cache
    assert_eq!(sink.len(), 3);

    let updates = sink.drain();
    assert!(sink.is_empty(), "drain clears the buffer");
    let applied = cache
        .writer()
        .transaction(move |tx| flush_last_used(tx, &updates))
        .await
        .expect("flush");
    assert_eq!(applied, 2, "only the two present rows are updated");

    let row_a = get_row(&cache, &a.blob_id).expect("a present");
    assert_eq!(row_a.last_used_at, 5000, "latest recorded timestamp wins");
    assert_eq!(row_a.created_at, 1000, "created_at is untouched");
    let row_b = get_row(&cache, &b.blob_id).expect("b present");
    assert_eq!(row_b.last_used_at, 6000);
    assert_eq!(row_b.created_at, 1000);
}

/// Column names of `table` via `PRAGMA table_info`.
fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    stmt.query_map([], |r| r.get::<_, String>(1))
        .expect("query table_info")
        .collect::<rusqlite::Result<Vec<String>>>()
        .expect("collect columns")
}
