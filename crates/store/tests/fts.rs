//! T08-01 acceptance tests: the FTS5 schema (`fts_doc`/`fts_occurrences`/
//! `fts_projection_head`, spec 03 §4.3) is live in a fresh `cache.sqlite`, the
//! `bm25` ranking function honors the declared column-weight order (spec 09
//! §2), and `fts_doc.fts_rowid`/`fts_occurrences.rowid` round-trip as the same
//! value. The pure tokenizer/manifest-hash golden tests live in
//! `cache::fts`'s own unit tests; these exercise the real SQLite schema
//! end-to-end via raw SQL (row insertion/materialization is T08-02's job, not
//! this task's).
//!
//! Deterministic: an isolated [`TempHome`], no network, no wall-clock sleeps.

use local_rag_core::paths::StoreLayout;
use local_rag_store::CacheDb;
use local_rag_store::rusqlite::{self, Connection};
use local_rag_test_support::TempHome;

const STORE_UUID: &str = "22222222-2222-7222-8222-222222222222";

/// A temp store with a freshly opened, bound `cache.sqlite`. The home is
/// returned to keep it alive.
fn open_cache() -> (TempHome, CacheDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let cache = CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite");
    (home, cache)
}

/// A distinct, deterministic 64-hex-char occurrence-id-shaped string.
fn occ(seed: u8) -> String {
    format!("{seed:02x}").repeat(32)
}

/// Insert one `fts_doc` + `fts_occurrences` row pair through the cache writer,
/// with an explicit `rowid` shared by both (spec 03 §4.3's join key).
#[allow(clippy::too_many_arguments)]
async fn insert_row(
    cache: &CacheDb,
    rowid: i64,
    occurrence_id: String,
    worktree_id: &str,
    generation_id: &str,
    name: &str,
    qualified_name: &str,
    path: &str,
    signature: &str,
    body: &str,
) {
    let (worktree_id, generation_id, name, qualified_name, path, signature, body) = (
        worktree_id.to_string(),
        generation_id.to_string(),
        name.to_string(),
        qualified_name.to_string(),
        path.to_string(),
        signature.to_string(),
        body.to_string(),
    );
    cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO fts_doc (fts_rowid, occurrence_id, worktree_id, generation_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![rowid, occurrence_id, worktree_id, generation_id],
            )?;
            tx.execute(
                "INSERT INTO fts_occurrences(rowid, name, qualified_name, path, signature, body) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![rowid, name, qualified_name, path, signature, body],
            )?;
            Ok(())
        })
        .await
        .expect("insert fts row pair")
}

/// `(occurrence_id, bm25 rank)` pairs matching `term`, weighted by `weights`
/// (`name, qualified_name, path, signature, body` order, spec 09 §2), best
/// match first (SQLite's `bm25()` is more negative for a better match).
fn ranks_for_term(read: &Connection, term: &str, weights: [f64; 5]) -> Vec<(String, f64)> {
    let mut stmt = read
        .prepare(
            "SELECT fts_doc.occurrence_id, \
                    bm25(fts_occurrences, ?2, ?3, ?4, ?5, ?6) AS rank \
             FROM fts_occurrences JOIN fts_doc ON fts_doc.fts_rowid = fts_occurrences.rowid \
             WHERE fts_occurrences MATCH ?1 ORDER BY rank",
        )
        .expect("prepare bm25 query");
    stmt.query_map(
        rusqlite::params![
            term, weights[0], weights[1], weights[2], weights[3], weights[4]
        ],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)),
    )
    .expect("run bm25 query")
    .collect::<rusqlite::Result<Vec<_>>>()
    .expect("collect bm25 rows")
}

const DEFAULT_WEIGHTS: [f64; 5] = [4.0, 3.0, 1.5, 2.0, 1.0];

// ---- FTS5 availability -------------------------------------------------------

#[tokio::test]
async fn fts5_module_is_compiled_in() {
    // If SQLite were not built with SQLITE_ENABLE_FTS5, `CacheDb::open` itself
    // would already fail inside `seed_binding`'s `CREATE VIRTUAL TABLE ...
    // USING fts5(...)`. This assertion makes the guard explicit and named,
    // rather than a confusing failure buried in cache-open plumbing.
    let (_home, cache) = open_cache();
    let read = cache.open_read().expect("open read-only cache");
    let enabled: i64 = read
        .query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
            r.get(0)
        })
        .expect("query compile option");
    assert_eq!(
        enabled, 1,
        "SQLite must be compiled with SQLITE_ENABLE_FTS5"
    );
}

#[test]
fn fts_tables_exist_after_open() {
    let (_home, cache) = open_cache();
    let read = cache.open_read().expect("open read-only cache");
    let mut stmt = read
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .expect("prepare sqlite_master query");
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .expect("query sqlite_master")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect table names");
    for expected in ["fts_doc", "fts_occurrences", "fts_projection_head"] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected table {expected:?} in {names:?}"
        );
    }
}

#[test]
fn fts_doc_by_wt_index_exists() {
    let (_home, cache) = open_cache();
    let read = cache.open_read().expect("open read-only cache");
    let mut stmt = read
        .prepare("PRAGMA index_list('fts_doc')")
        .expect("prepare index_list");
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(1)) // column 1 = index name
        .expect("query index_list")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect index names");
    assert!(
        names.iter().any(|n| n == "fts_doc_by_wt"),
        "expected fts_doc_by_wt in {names:?}"
    );
}

// ---- rowid linkage ------------------------------------------------------------

#[tokio::test]
async fn fts_doc_rowid_round_trips_as_fts_occurrences_rowid() {
    let (_home, cache) = open_cache();
    insert_row(
        &cache,
        42,
        occ(1),
        "wt",
        "gen",
        "extractImports",
        "",
        "",
        "",
        "",
    )
    .await;

    let read = cache.open_read().expect("open read-only cache");
    let occurrence_rowid: i64 = read
        .query_row(
            "SELECT rowid FROM fts_occurrences WHERE name = ?1",
            rusqlite::params!["extractImports"],
            |r| r.get(0),
        )
        .expect("read fts_occurrences rowid");
    let doc_rowid: i64 = read
        .query_row(
            "SELECT fts_rowid FROM fts_doc WHERE occurrence_id = ?1",
            rusqlite::params![occ(1)],
            |r| r.get(0),
        )
        .expect("read fts_doc.fts_rowid");
    assert_eq!(occurrence_rowid, 42);
    assert_eq!(doc_rowid, 42);
    assert_eq!(occurrence_rowid, doc_rowid);
}

#[tokio::test]
async fn fts_doc_occurrence_id_is_unique() {
    let (_home, cache) = open_cache();
    insert_row(&cache, 1, occ(1), "wt", "gen", "a", "", "", "", "").await;

    let (occurrence_id, worktree_id, generation_id) = (occ(1), "wt".to_string(), "gen".to_string());
    let err = cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO fts_doc (fts_rowid, occurrence_id, worktree_id, generation_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![2i64, occurrence_id, worktree_id, generation_id],
            )
        })
        .await;
    assert!(err.is_err(), "duplicate occurrence_id must be rejected");
}

// ---- bm25 columns -------------------------------------------------------------

#[tokio::test]
async fn bm25_weights_favor_the_declared_column_order() {
    let (_home, cache) = open_cache();
    // Term "sentinel" appears only in `name` for row A, only in `body` for row B.
    insert_row(
        &cache,
        1,
        occ(1),
        "wt",
        "gen",
        "sentinel",
        "",
        "",
        "",
        "unrelated words here",
    )
    .await;
    insert_row(
        &cache,
        2,
        occ(2),
        "wt",
        "gen",
        "unrelated",
        "",
        "",
        "",
        "sentinel words here",
    )
    .await;

    let read = cache.open_read().expect("open read-only cache");
    let ranks = ranks_for_term(&read, "sentinel", DEFAULT_WEIGHTS);
    assert_eq!(ranks.len(), 2, "both rows must match: {ranks:?}");
    assert_eq!(
        ranks[0].0,
        occ(1),
        "a name-column match must outrank a body-column match with default weights {DEFAULT_WEIGHTS:?}: {ranks:?}"
    );
}

#[tokio::test]
async fn bm25_weight_order_is_actually_sensitive() {
    let (_home, cache) = open_cache();
    insert_row(
        &cache,
        1,
        occ(1),
        "wt",
        "gen",
        "sentinel",
        "",
        "",
        "",
        "unrelated words here",
    )
    .await;
    insert_row(
        &cache,
        2,
        occ(2),
        "wt",
        "gen",
        "unrelated",
        "",
        "",
        "",
        "sentinel words here",
    )
    .await;

    let read = cache.open_read().expect("open read-only cache");
    // Reversed weights: body now outweighs name. A silently-transposed weight
    // argument in production code would not be caught by the previous test
    // alone (any 5 numbers "pass" if the assertion only checks *a* ranking);
    // this proves the ranking is actually sensitive to argument order.
    let reversed = [1.0, 2.0, 1.5, 3.0, 4.0];
    let ranks = ranks_for_term(&read, "sentinel", reversed);
    assert_eq!(
        ranks[0].0,
        occ(2),
        "with body weighted highest, the body match must rank first: {ranks:?}"
    );
}

#[tokio::test]
async fn bm25_rewards_multi_column_hits() {
    let (_home, cache) = open_cache();
    insert_row(
        &cache,
        1,
        occ(1),
        "wt",
        "gen",
        "apples",
        "",
        "",
        "",
        "oranges",
    )
    .await;
    insert_row(
        &cache,
        2,
        occ(2),
        "wt",
        "gen",
        "apples",
        "",
        "",
        "",
        "apples",
    )
    .await;

    let read = cache.open_read().expect("open read-only cache");
    let ranks = ranks_for_term(&read, "apples", DEFAULT_WEIGHTS);
    let rank_of = |id: &str| ranks.iter().find(|(o, _)| o == id).expect("row present").1;
    assert!(
        rank_of(&occ(2)) < rank_of(&occ(1)),
        "a hit in two columns must rank at least as well as a hit in one: {ranks:?}"
    );
}
