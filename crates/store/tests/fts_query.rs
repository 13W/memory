//! T12-01 acceptance tests: the lexical leg of spec 09 §1 — the active-generation
//! BM25 query with the spec's default column weights (§2), code-aware query
//! preprocessing, the `name_pattern` prefix filter, and the `max(limit·4, 50)`
//! candidate depth (§4).
//!
//! Two fixture families, on purpose:
//!
//! * **directly seeded `fts_doc`/`fts_occurrences` rows** (the `insert_row`
//!   helper, copied from `crates/store/tests/fts.rs`) — total control over each
//!   of the five ranked columns, which is what a weight-ordering golden needs,
//!   and the only way to exercise the `signature` column at all: the generation
//!   materializer writes `tokenize_signature(&[])` for every row today, since
//!   raw parameter/return-type text is not yet plumbed out of the tree-sitter
//!   adapters (spec 09 §2 / 06 §4 as-built notes, T08-02). T12-01 owns the
//!   *query*, not the ingest, so the weight is proven here and starts acting on
//!   production ranking unchanged once that column is populated.
//! * **a real [`materialize_fts`] generation** (the `seed_*` helpers, copied
//!   from `crates/store/tests/fts_materialize.rs`) — proves the query's
//!   tokenization actually meets the *indexer's* tokenization on real rows, and
//!   that every `UnitKind` is searchable (v1 parity `[FIXED]`, spec 09 §1).
//!
//! Integration test binaries can't share code without a `mod` file, so this
//! duplicates those helpers rather than importing them — the same convention
//! `fts.rs`/`fts_materialize.rs`/`fts_corruption.rs` already follow.
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, no network,
//! no wall-clock sleeps.

use std::collections::HashSet;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::code::{
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, UnitKind,
};
use local_rag_store::registry::{WorktreeKind, create_repository, create_worktree};
use local_rag_store::rusqlite::{self, Connection};
use local_rag_store::{
    BM25_DEFAULT_WEIGHTS, CacheDb, LexicalHit, LexicalQuery, StateDb, candidate_depth,
    derive_content_blob, fts_match_expression, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, lexical_leg, materialize_fts,
    occurrence_id, query_fts,
};
use local_rag_test_support::TempHome;

const STORE_UUID: &str = "44444444-4444-7444-8444-444444444444";
const NOW: i64 = 1_000_000;

// ---- fixtures: directly seeded FTS rows -------------------------------------

/// A temp store with a freshly opened, bound `cache.sqlite`.
fn open_cache() -> (TempHome, CacheDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let cache = CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite");
    (home, cache)
}

/// A temp store with an ensured tree plus opened `state.sqlite` and
/// `cache.sqlite`.
fn open_both() -> (TempHome, StateDb, CacheDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let cache = CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite");
    (home, state, cache)
}

/// A distinct, deterministic 64-hex-char occurrence-id-shaped string.
fn occ(seed: u8) -> String {
    format!("{seed:02x}").repeat(32)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// The five ranked columns of one row, in `bm25()` declaration order.
#[derive(Default)]
struct Columns<'a> {
    name: &'a str,
    qualified_name: &'a str,
    path: &'a str,
    signature: &'a str,
    body: &'a str,
}

/// Insert one `fts_doc` + `fts_occurrences` row pair through the cache writer,
/// with an explicit `rowid` shared by both (spec 03 §4.3's join key).
async fn insert_row(
    cache: &CacheDb,
    rowid: i64,
    occurrence_id: &str,
    worktree_id: &str,
    generation_id: &str,
    columns: Columns<'_>,
) {
    let (occurrence_id, worktree_id, generation_id) = (
        occurrence_id.to_string(),
        worktree_id.to_string(),
        generation_id.to_string(),
    );
    let (name, qualified_name, path, signature, body) = (
        columns.name.to_string(),
        columns.qualified_name.to_string(),
        columns.path.to_string(),
        columns.signature.to_string(),
        columns.body.to_string(),
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

/// Run the lexical leg against `cache` for `(worktree, generation)`.
fn leg(
    cache: &CacheDb,
    worktree_id: &str,
    generation_id: &str,
    query: &str,
    name_pattern: Option<&str>,
    limit: usize,
) -> Vec<LexicalHit> {
    let read = cache.open_read().expect("cache read conn");
    lexical_leg(
        &read,
        worktree_id,
        generation_id,
        &LexicalQuery::new(query, name_pattern, limit),
    )
    .expect("lexical leg")
}

fn ids(hits: &[LexicalHit]) -> Vec<String> {
    hits.iter().map(|h| h.occurrence_id.clone()).collect()
}

// ---- fixtures: a real materialized generation -------------------------------

/// Create a repository and one `active` main worktree; returns the worktree id.
async fn seed_worktree(state: &StateDb, seed: u8) -> String {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(1));
    let (r, w) = (repo, wt.clone());
    state
        .writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, NOW)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, NOW)
        })
        .await
        .expect("seed worktree");
    wt
}

/// Insert a bare `generation` row directly (the generation builder is group 05;
/// only a valid FK parent is needed here).
async fn seed_generation(state: &StateDb, worktree_id: &str, generation_id: &str, number: i64) {
    let (w, g) = (worktree_id.to_string(), generation_id.to_string());
    state
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO generation \
                   (generation_id, worktree_id, generation_number, state, created_at) \
                 VALUES (?1, ?2, ?3, 'active', ?4)",
                rusqlite::params![g, w, number, NOW],
            )
            .map(|_| ())
        })
        .await
        .expect("seed generation");
}

/// Insert one `file_revision` + `content_blob` + `parsed_unit` for `content`
/// (the unit spans the whole content, real content-addressed `blob_id`).
async fn seed_file_content(
    state: &StateDb,
    file_revision_id: &str,
    unit_id: &str,
    unit_kind: UnitKind,
    content: &str,
    local_name: Option<&str>,
) {
    let derived = derive_content_blob("rust", content);
    let (fr, u, blob, bytes, name) = (
        file_revision_id.to_string(),
        unit_id.to_string(),
        derived.blob_id.clone(),
        content.as_bytes().to_vec(),
        local_name.map(str::to_string),
    );
    let len = bytes.len() as i64;
    let (algo, norm) = (derived.algo_version, derived.normalization_version);
    state
        .writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &fr,
                    content_hash: &fr,
                    parser_fingerprint: "test-fp",
                    source_blob: &bytes,
                    compression: local_rag_store::SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: local_rag_store::NewlineStyle::Lf,
                    source_size: len,
                },
                NOW,
            )?;
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &blob,
                    language: "rust",
                    algo_version: algo,
                    normalization_version: norm,
                },
                NOW,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u,
                    file_revision_id: &fr,
                    unit_kind,
                    syntax_locator: &format!("loc:{u}"),
                    blob_id: &blob,
                    span_start: 0,
                    span_end: len,
                    local_name: name.as_deref(),
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect("seed file content");
}

/// Bind `unit_id` to `normalized_path` as a member+occurrence of
/// `generation_id`. Returns the occurrence id.
async fn seed_occurrence(
    state: &StateDb,
    generation_id: &str,
    normalized_path: &str,
    file_revision_id: &str,
    unit_id: &str,
) -> String {
    let occ = occurrence_id(generation_id, normalized_path, unit_id);
    let (g, path, fr, u, o) = (
        generation_id.to_string(),
        normalized_path.to_string(),
        file_revision_id.to_string(),
        unit_id.to_string(),
        occ.clone(),
    );
    state
        .writer()
        .transaction(move |tx| {
            insert_generation_file(tx, &g, &path, &path, &fr)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &o,
                    generation_id: &g,
                    normalized_path: &path,
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

// ---- ranking goldens (spec 09 §2) -------------------------------------------

/// The five default weights (`4.0, 3.0, 1.5, 2.0, 1.0`) must produce exactly
/// the ranking order `name > qualified_name > signature > path > body` for one
/// and the same term. Every row carries exactly one token per column, so BM25's
/// own length normalization is identical across them and the *only* thing
/// separating the five is the column weight.
///
/// The 20 filler rows are not decoration: BM25's IDF term goes negative when a
/// term appears in (nearly) every document, which would invert the ordering.
/// They keep the matched term rare, as it is on any real corpus.
#[tokio::test]
async fn default_weights_rank_name_above_qualified_signature_path_and_body() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);

    let target = "zephyr";
    let filler = "alpha";
    let by_column = [
        Columns {
            name: target,
            qualified_name: filler,
            path: filler,
            signature: filler,
            body: filler,
        },
        Columns {
            name: filler,
            qualified_name: target,
            path: filler,
            signature: filler,
            body: filler,
        },
        Columns {
            name: filler,
            qualified_name: filler,
            path: filler,
            signature: target,
            body: filler,
        },
        Columns {
            name: filler,
            qualified_name: filler,
            path: target,
            signature: filler,
            body: filler,
        },
        Columns {
            name: filler,
            qualified_name: filler,
            path: filler,
            signature: filler,
            body: target,
        },
    ];
    // Seeded in the *expected ranking* order, but with occurrence ids assigned
    // in the opposite order, so a passing test cannot be explained by insertion
    // order or by the `occurrence_id ASC` tie-break.
    let expected: Vec<String> = (0..5).map(|i| occ(200 - i)).collect();
    for (i, columns) in by_column.into_iter().enumerate() {
        insert_row(
            &cache,
            i as i64 + 1,
            &expected[i],
            &wt,
            &generation,
            columns,
        )
        .await;
    }
    for i in 0..20 {
        insert_row(
            &cache,
            100 + i as i64,
            &occ(i),
            &wt,
            &generation,
            Columns {
                name: filler,
                qualified_name: filler,
                path: filler,
                signature: filler,
                body: filler,
            },
        )
        .await;
    }

    let hits = leg(&cache, &wt, &generation, target, None, 10);
    assert_eq!(
        ids(&hits),
        expected,
        "column weights 4.0/3.0/2.0/1.5/1.0 must order name > qualified > signature > path > body"
    );
    assert_eq!(
        hits.iter().map(|h| h.rank).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "ranks are 1-based and dense"
    );
    assert!(
        hits.windows(2).all(|w| w[0].bm25 <= w[1].bm25),
        "SQLite bm25() is more negative for a better match, so it ascends with rank: {:?}",
        hits.iter().map(|h| h.bm25).collect::<Vec<_>>()
    );
}

/// The `signature` column really participates in ranking — a row matching only
/// through `signature` outranks one matching only through `path` or `body`.
/// See the module docs: this is the column the materializer leaves empty today,
/// so the query-side guarantee is proven here rather than end to end.
#[tokio::test]
async fn signature_column_outranks_path_and_body() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);

    let sig_row = occ(1);
    let path_row = occ(2);
    let body_row = occ(3);
    insert_row(
        &cache,
        1,
        &sig_row,
        &wt,
        &generation,
        Columns {
            signature: "usize serialize",
            ..Columns::default()
        },
    )
    .await;
    insert_row(
        &cache,
        2,
        &path_row,
        &wt,
        &generation,
        Columns {
            path: "usize serialize",
            ..Columns::default()
        },
    )
    .await;
    insert_row(
        &cache,
        3,
        &body_row,
        &wt,
        &generation,
        Columns {
            body: "usize serialize",
            ..Columns::default()
        },
    )
    .await;
    for i in 0..20 {
        insert_row(
            &cache,
            100 + i as i64,
            &occ(50 + i),
            &wt,
            &generation,
            Columns {
                body: "unrelated filler text",
                ..Columns::default()
            },
        )
        .await;
    }

    let hits = leg(&cache, &wt, &generation, "serialize", None, 10);
    assert_eq!(ids(&hits), vec![sig_row, path_row, body_row]);
}

/// Equal-scoring rows are ordered by `occurrence_id ASC` — spec 09 §4's
/// deterministic tie-break, applied inside the leg so that a truncation at
/// candidate depth is reproducible instead of storage-order dependent.
#[tokio::test]
async fn identical_rows_break_ties_by_occurrence_id() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);

    // Inserted in descending id order; the leg must return them ascending.
    for (rowid, seed) in [(1i64, 30u8), (2, 20), (3, 10)] {
        insert_row(
            &cache,
            rowid,
            &occ(seed),
            &wt,
            &generation,
            Columns {
                name: "duplicate",
                ..Columns::default()
            },
        )
        .await;
    }

    let hits = leg(&cache, &wt, &generation, "duplicate", None, 10);
    assert_eq!(ids(&hits), vec![occ(10), occ(20), occ(30)]);
    // Same input, same output — byte-for-byte repeatable.
    assert_eq!(
        ids(&leg(&cache, &wt, &generation, "duplicate", None, 10)),
        ids(&hits)
    );
}

// ---- scoping (spec 06 §3, 09 §1) --------------------------------------------

/// Rows of another worktree, or of a non-active generation of the *same*
/// worktree, are never returned. The generation predicate is defence in depth
/// behind `fts_projection_head` validation: even a stale head that somehow
/// passed cannot leak another generation's occurrences into a response
/// (spec 06 §3 "the read lock prevents mixing").
#[tokio::test]
async fn other_worktrees_and_generations_are_never_returned() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let other_wt = uuid(2);
    let active = uuid(3);
    let stale = uuid(4);

    let wanted = occ(1);
    insert_row(
        &cache,
        1,
        &wanted,
        &wt,
        &active,
        Columns {
            name: "shared",
            ..Columns::default()
        },
    )
    .await;
    insert_row(
        &cache,
        2,
        &occ(2),
        &wt,
        &stale,
        Columns {
            name: "shared",
            ..Columns::default()
        },
    )
    .await;
    insert_row(
        &cache,
        3,
        &occ(3),
        &other_wt,
        &active,
        Columns {
            name: "shared",
            ..Columns::default()
        },
    )
    .await;

    assert_eq!(
        ids(&leg(&cache, &wt, &active, "shared", None, 10)),
        vec![wanted]
    );
    // And the stale generation is reachable only when it is the one asked for —
    // proving the filter is a real predicate, not an accident of the fixture.
    assert_eq!(
        ids(&leg(&cache, &wt, &stale, "shared", None, 10)),
        vec![occ(2)]
    );
}

// ---- candidate depth (spec 09 §4) -------------------------------------------

/// `limit = 1` ⇒ depth `max(4, 50) = 50`: the leg returns exactly 50 of the 60
/// matching rows, cut along the deterministic order, and repeats identically.
#[tokio::test]
async fn candidate_depth_truncates_at_the_floor_deterministically() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);

    for i in 0..60u8 {
        insert_row(
            &cache,
            i as i64 + 1,
            &occ(i),
            &wt,
            &generation,
            Columns {
                name: "widespread",
                ..Columns::default()
            },
        )
        .await;
    }

    assert_eq!(candidate_depth(1), 50);
    let hits = leg(&cache, &wt, &generation, "widespread", None, 1);
    assert_eq!(hits.len(), 50);
    assert_eq!(hits.last().expect("50 hits").rank, 50);
    // All rows score identically, so the cut is purely the `occurrence_id ASC`
    // tie-break: the 50 lowest ids, in order.
    let mut all: Vec<String> = (0..60u8).map(occ).collect();
    all.sort();
    assert_eq!(ids(&hits), all[..50]);
    assert_eq!(
        ids(&leg(&cache, &wt, &generation, "widespread", None, 1)),
        ids(&hits)
    );

    // A larger limit lifts the floor: `13 * 4 = 52`.
    assert_eq!(
        leg(&cache, &wt, &generation, "widespread", None, 13).len(),
        52
    );
}

// ---- name_pattern (spec 09 §1) ----------------------------------------------

#[tokio::test]
async fn name_pattern_prefix_filters_by_name_and_qualified_name() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);

    let by_name = occ(1);
    let by_qualified = occ(2);
    let unrelated = occ(3);
    insert_row(
        &cache,
        1,
        &by_name,
        &wt,
        &generation,
        Columns {
            name: "extractimports extract imports",
            body: "parser helper",
            ..Columns::default()
        },
    )
    .await;
    insert_row(
        &cache,
        2,
        &by_qualified,
        &wt,
        &generation,
        Columns {
            name: "run",
            qualified_name: "extractor",
            body: "parser helper",
            ..Columns::default()
        },
    )
    .await;
    insert_row(
        &cache,
        3,
        &unrelated,
        &wt,
        &generation,
        Columns {
            name: "compile",
            body: "parser helper",
            ..Columns::default()
        },
    )
    .await;

    // Without a filter all three match the body term.
    let all = ids(&leg(&cache, &wt, &generation, "parser", None, 10));
    assert_eq!(all.len(), 3);

    // The prefix `extr` reaches both `extract…` (name) and `extractor`
    // (qualified_name), and excludes `compile`.
    let filtered = ids(&leg(&cache, &wt, &generation, "parser", Some("extr"), 10));
    assert_eq!(
        filtered.iter().collect::<HashSet<_>>(),
        [&by_name, &by_qualified]
            .into_iter()
            .collect::<HashSet<_>>()
    );

    // A `body`-only term is *not* reachable through the filter: the column
    // scope really is `{name qualified_name}`.
    assert!(leg(&cache, &wt, &generation, "parser", Some("parser"), 10).is_empty());
}

#[tokio::test]
async fn multi_token_pattern_requires_every_prefix() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);

    let both = occ(1);
    let only_one = occ(2);
    insert_row(
        &cache,
        1,
        &both,
        &wt,
        &generation,
        Columns {
            name: "extractimports extract imports",
            body: "shared",
            ..Columns::default()
        },
    )
    .await;
    insert_row(
        &cache,
        2,
        &only_one,
        &wt,
        &generation,
        Columns {
            name: "extractor",
            body: "shared",
            ..Columns::default()
        },
    )
    .await;

    // `extractImp` tokenizes to `extractimp` + `extract` + `imp`; every one of
    // them must be a prefix of some indexed name token.
    assert_eq!(
        ids(&leg(
            &cache,
            &wt,
            &generation,
            "shared",
            Some("extractImp"),
            10
        )),
        vec![both]
    );
}

#[tokio::test]
async fn pattern_without_matches_is_an_empty_result_not_an_error() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);
    insert_row(
        &cache,
        1,
        &occ(1),
        &wt,
        &generation,
        Columns {
            name: "compile",
            body: "shared",
            ..Columns::default()
        },
    )
    .await;

    assert!(leg(&cache, &wt, &generation, "shared", Some("zzz"), 10).is_empty());
}

#[tokio::test]
async fn empty_pattern_does_not_filter_and_pattern_only_query_is_valid() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);
    let row = occ(1);
    insert_row(
        &cache,
        1,
        &row,
        &wt,
        &generation,
        Columns {
            name: "compile",
            body: "shared",
            ..Columns::default()
        },
    )
    .await;

    assert_eq!(
        ids(&leg(&cache, &wt, &generation, "shared", Some(""), 10)),
        vec![row.clone()],
        "an empty pattern is no filter, not an impossible one"
    );
    assert_eq!(
        ids(&leg(&cache, &wt, &generation, "", Some("comp"), 10)),
        vec![row],
        "a pattern with no query is a valid filter-only search"
    );
}

/// A query that reduces to no terms at all must not touch SQLite: the
/// connection here has no FTS tables whatsoever, so any statement would fail.
#[test]
fn a_termless_query_runs_no_sql() {
    let conn = Connection::open_in_memory().expect("in-memory conn");
    for (query, pattern) in [
        ("", None),
        ("   ", None),
        ("///", Some("")),
        ("", Some("  ")),
    ] {
        assert_eq!(fts_match_expression(query, pattern), None);
        assert_eq!(
            lexical_leg(&conn, "wt", "gen", &LexicalQuery::new(query, pattern, 10))
                .expect("no SQL, no error"),
            Vec::new()
        );
    }
}

/// FTS5 reads bare `AND`/`OR`/`NOT`/`NEAR` as operators; the leg quotes every
/// term, so a query made entirely of them parses and matches as words.
#[tokio::test]
async fn fts5_operator_words_are_searchable_terms_not_syntax_errors() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);
    let row = occ(1);
    insert_row(
        &cache,
        1,
        &row,
        &wt,
        &generation,
        Columns {
            body: "and or not near",
            ..Columns::default()
        },
    )
    .await;

    assert_eq!(
        ids(&leg(&cache, &wt, &generation, "near", None, 10)),
        vec![row.clone()]
    );
    assert_eq!(
        ids(&leg(&cache, &wt, &generation, "and or not near", None, 10)),
        vec![row]
    );
}

// ---- against a real materialized generation ---------------------------------

/// The query's tokenization meets the indexer's: a `camelCase` symbol is
/// findable by its whole name, by either part, and through its path components
/// — without the test ever restating a token by hand.
#[tokio::test]
async fn identifiers_and_path_components_match_a_materialized_generation() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 1).await;
    let generation = uuid(10);
    seed_generation(&state, &wt, &generation, 1).await;

    let rev = uuid(11);
    let unit = uuid(12);
    seed_file_content(
        &state,
        &rev,
        &unit,
        UnitKind::Symbol,
        "fn extract_imports() {}",
        Some("extractImports"),
    )
    .await;
    let occ_id = seed_occurrence(&state, &generation, "src/parser/index.ts", &rev, &unit).await;

    // A second, unrelated occurrence so a passing assertion means "found this
    // one", not "returned everything".
    let other_rev = uuid(13);
    let other_unit = uuid(14);
    seed_file_content(
        &state,
        &other_rev,
        &other_unit,
        UnitKind::Symbol,
        "fn compile() {}",
        Some("compileModule"),
    )
    .await;
    seed_occurrence(
        &state,
        &generation,
        "src/build/compile.ts",
        &other_rev,
        &other_unit,
    )
    .await;

    materialize_fts(&state, &cache, &wt, &generation, NOW)
        .await
        .expect("materialize");

    for query in ["extractImports", "extract", "imports", "extract_imports"] {
        assert_eq!(
            ids(&leg(&cache, &wt, &generation, query, None, 10)),
            vec![occ_id.clone()],
            "query {query:?} must find the camelCase symbol"
        );
    }
    // Path components are indexed as their own column.
    assert_eq!(
        ids(&leg(&cache, &wt, &generation, "parser", None, 10)),
        vec![occ_id.clone()]
    );
    // …and `name_pattern` filters real rows, not only hand-seeded ones.
    assert_eq!(
        ids(&leg(
            &cache,
            &wt,
            &generation,
            "parser",
            Some("extractImp"),
            10
        )),
        vec![occ_id]
    );
}

/// Indexed population is *document units of all kinds* — "anything less is a
/// parity regression vs v1" (spec 09 §1 `[FIXED]`). Every `UnitKind` must be
/// reachable through the lexical leg, not only `Symbol`.
#[tokio::test]
async fn every_unit_kind_is_searchable() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 1).await;
    let generation = uuid(10);
    seed_generation(&state, &wt, &generation, 1).await;

    let kinds = [
        UnitKind::Symbol,
        UnitKind::File,
        UnitKind::ConfigSection,
        UnitKind::TextSection,
        UnitKind::FallbackChunk,
    ];
    let mut expected = HashSet::new();
    for (i, kind) in kinds.into_iter().enumerate() {
        let seed = 20 + i as u8 * 2;
        let rev = uuid(seed);
        let unit = uuid(seed + 1);
        // Distinct content per kind (a shared blob would collapse them into one
        // content-addressed row), each carrying the same searchable term.
        seed_file_content(
            &state,
            &rev,
            &unit,
            kind,
            &format!("marker term for {} unit", kind.as_str()),
            Some(kind.as_str()),
        )
        .await;
        expected.insert(
            seed_occurrence(
                &state,
                &generation,
                &format!("src/{}.rs", kind.as_str()),
                &rev,
                &unit,
            )
            .await,
        );
    }

    materialize_fts(&state, &cache, &wt, &generation, NOW)
        .await
        .expect("materialize");

    let hits = leg(&cache, &wt, &generation, "marker", None, 10);
    assert_eq!(
        ids(&hits).into_iter().collect::<HashSet<_>>(),
        expected,
        "all five unit kinds must be searchable (v1 parity [FIXED])"
    );
}

/// `query_fts` is the raw ranking primitive: same rows, explicitly supplied
/// weights. Flipping the defaults on their head flips the ranking — proof the
/// weights are genuinely applied rather than a decorative constant.
#[tokio::test]
async fn explicit_weights_are_applied_by_the_ranking_query() {
    let (_home, cache) = open_cache();
    let wt = uuid(1);
    let generation = uuid(2);
    let name_row = occ(1);
    let body_row = occ(2);
    insert_row(
        &cache,
        1,
        &name_row,
        &wt,
        &generation,
        Columns {
            name: "zephyr",
            ..Columns::default()
        },
    )
    .await;
    insert_row(
        &cache,
        2,
        &body_row,
        &wt,
        &generation,
        Columns {
            body: "zephyr",
            ..Columns::default()
        },
    )
    .await;
    for i in 0..20u8 {
        insert_row(
            &cache,
            100 + i as i64,
            &occ(50 + i),
            &wt,
            &generation,
            Columns {
                body: "filler",
                ..Columns::default()
            },
        )
        .await;
    }

    let read = cache.open_read().expect("cache read conn");
    let expr = fts_match_expression("zephyr", None).expect("expression");

    let defaults = query_fts(&read, &wt, &generation, &expr, BM25_DEFAULT_WEIGHTS, 50)
        .expect("default-weighted query");
    assert_eq!(ids(&defaults), vec![name_row.clone(), body_row.clone()]);

    let inverted = query_fts(
        &read,
        &wt,
        &generation,
        &expr,
        [1.0, 1.0, 1.0, 1.0, 4.0],
        50,
    )
    .expect("inverted-weight query");
    assert_eq!(ids(&inverted), vec![body_row, name_row]);
}
