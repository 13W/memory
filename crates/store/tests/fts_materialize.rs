//! T08-02 acceptance tests: the generation materializer ([`materialize_fts`])
//! reads a generation's occurrences from `state.sqlite`, resolves/recomputes
//! their normalized body text, and replaces a worktree's `fts_doc`/
//! `fts_occurrences` rows with the new generation's complete set, writing
//! `fts_projection_head` last inside one cache transaction (spec 06 §2/§4).
//!
//! Every test builds its `state.sqlite` fixtures directly through
//! `local-rag-store`'s own low-level primitives (`insert_file_revision`,
//! `insert_content_blob`, `insert_parsed_unit`, `insert_generation_file`,
//! `insert_occurrence`) — no `crates/index` parser/scan involvement (matching
//! T08-01's self-containment discipline; a full generation-diff JSON fixture
//! family does not exist, `fixtures/reconcile/index.json`'s own description
//! flags this as the open "GAP-02").
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, no
//! network, no wall-clock sleeps. All tests that call [`materialize_fts`]
//! serialize on [`SERIAL`] because the `cache:fts_before_head` failpoint is a
//! process-global registry (one per test *binary*, not per test): while it is
//! potentially armed by `fail_before_head_rolls_back_whole_tx`, no other test
//! in this binary may run a concurrent `materialize_fts` call (same class of
//! hazard as `crates/projection/tests/fault_matrix.rs`'s `SERIAL` guard).

use std::collections::HashSet;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::code::{
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, UnitKind,
};
use local_rag_store::registry::{WorktreeKind, create_repository, create_worktree};
use local_rag_store::rusqlite;
use local_rag_store::{
    CacheDb, StateDb, derive_content_blob, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, materialize_fts, occurrence_id,
    read_fts_projection_head,
};
#[cfg(feature = "failpoints")]
use local_rag_test_support::Action;
use local_rag_test_support::TempHome;
use tokio::sync::Mutex;

/// Serializes every test that calls [`materialize_fts`] — see module docs.
static SERIAL: Mutex<()> = Mutex::const_new(());

const STORE_UUID: &str = "33333333-3333-7333-8333-333333333333";
const NOW: i64 = 1_000_000;

// ---- helpers ----------------------------------------------------------------

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

/// A distinct, deterministic UUIDv7 string keyed by `seed`.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

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

/// Insert a bare `generation` row directly (the generation builder is group
/// 05; only a valid FK parent is needed here).
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
/// (the unit spans the whole content, `[0, content.len())`, real content-
/// addressed `blob_id` via [`derive_content_blob`] — so the materializer's
/// recompute path always re-derives the identical id, never a spurious
/// `BlobMismatch`). Returns `blob_id`.
async fn seed_file_content(
    state: &StateDb,
    file_revision_id: &str,
    unit_id: &str,
    unit_kind: UnitKind,
    content: &str,
    local_name: Option<&str>,
) -> String {
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
    derived.blob_id
}

/// Bind `unit_id` to `normalized_path` as a member+occurrence of `generation_id`
/// (deterministic `occurrence_id`, spec 03 §1.2). Returns the occurrence id.
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

/// Every `name` token stored in `fts_occurrences` (space-joined column, split
/// back into a set of raw column values for membership assertions).
fn fts_names(cache: &CacheDb) -> HashSet<String> {
    let read = cache.open_read().expect("read conn");
    let mut stmt = read
        .prepare("SELECT name FROM fts_occurrences")
        .expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<rusqlite::Result<HashSet<_>>>()
        .expect("collect")
}

fn fts_row_count(cache: &CacheDb) -> i64 {
    let read = cache.open_read().expect("read conn");
    read.query_row("SELECT COUNT(*) FROM fts_doc", [], |r| r.get(0))
        .expect("count")
}

#[cfg(feature = "failpoints")]
fn arm(name: &str) {
    let fp = local_rag_test_support::failpoint::global();
    fp.register(name);
    fp.arm(name, Action::Error).expect("arm failpoint");
}

#[cfg(feature = "failpoints")]
fn disarm(name: &str) {
    local_rag_test_support::failpoint::global()
        .disarm(name)
        .expect("disarm failpoint");
}

// ---- tests --------------------------------------------------------------------

#[tokio::test]
async fn a_to_b_add_rename_delete_reflects_in_materialized_fts() {
    let _serial = SERIAL.lock().await;
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 1).await;

    // Generation A: foo.rs, bar.rs.
    let gen_a = uuid(10);
    seed_generation(&state, &wt, &gen_a, 1).await;
    let foo_rev = uuid(11);
    seed_file_content(
        &state,
        &foo_rev,
        &uuid(12),
        UnitKind::Symbol,
        "fn foo() {}",
        Some("foo"),
    )
    .await;
    seed_occurrence(&state, &gen_a, "foo.rs", &foo_rev, &uuid(12)).await;
    let bar_rev = uuid(13);
    let bar_unit = uuid(14);
    seed_file_content(
        &state,
        &bar_rev,
        &bar_unit,
        UnitKind::Symbol,
        "fn bar() {}",
        Some("bar"),
    )
    .await;
    seed_occurrence(&state, &gen_a, "bar.rs", &bar_rev, &bar_unit).await;

    let outcome_a = materialize_fts(&state, &cache, &wt, &gen_a, NOW)
        .await
        .expect("materialize A");
    assert_eq!(outcome_a.occurrence_count, 2);
    assert_eq!(
        fts_names(&cache),
        HashSet::from(["foo".to_string(), "bar".to_string()])
    );

    // Generation B: bar.rs renamed to baz.rs (same file_revision/unit — content
    // unchanged, structural sharing), foo.rs deleted, qux.rs added.
    let gen_b = uuid(20);
    seed_generation(&state, &wt, &gen_b, 2).await;
    seed_occurrence(&state, &gen_b, "baz.rs", &bar_rev, &bar_unit).await;
    let qux_rev = uuid(21);
    seed_file_content(
        &state,
        &qux_rev,
        &uuid(22),
        UnitKind::Symbol,
        "fn qux() {}",
        Some("qux"),
    )
    .await;
    seed_occurrence(&state, &gen_b, "qux.rs", &qux_rev, &uuid(22)).await;

    let outcome_b = materialize_fts(&state, &cache, &wt, &gen_b, NOW)
        .await
        .expect("materialize B");
    assert_eq!(outcome_b.occurrence_count, 2);
    assert_ne!(
        outcome_a.manifest_hash, outcome_b.manifest_hash,
        "different occurrence sets must yield different manifests"
    );

    // Full replace, not merge: only B's occurrences remain.
    assert_eq!(
        fts_names(&cache),
        HashSet::from(["bar".to_string(), "qux".to_string()]),
        "foo (deleted) must be gone; bar (renamed, same unit) and qux (added) present"
    );
    assert_eq!(fts_row_count(&cache), 2);

    let head = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    assert_eq!(head.generation_id, gen_b);
    assert_eq!(head.occurrence_count, 2);
    assert_eq!(head.manifest_hash, outcome_b.manifest_hash);
}

#[tokio::test]
async fn all_unit_kinds_present_are_materialized_agnostically() {
    let _serial = SERIAL.lock().await;
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 30).await;
    let gen_id = uuid(40);
    seed_generation(&state, &wt, &gen_id, 1).await;

    // Single-word labels (no `_`/`-`) so the tokenizer keeps each one as one
    // unsplit `name` token — the assertion below checks for these tokens
    // verbatim, not the app-side identifier-splitting behavior (that is
    // T08-01's own, separately tested, concern).
    let kinds = [
        (UnitKind::Symbol, "symbolkind"),
        (UnitKind::File, "filekind"),
        (UnitKind::ConfigSection, "configkind"),
        (UnitKind::TextSection, "textkind"),
        (UnitKind::FallbackChunk, "fallbackkind"),
    ];
    for (i, (kind, label)) in kinds.iter().enumerate() {
        let seed = 41 + (i as u8) * 3;
        let rev = uuid(seed);
        let unit = uuid(seed + 1);
        seed_file_content(
            &state,
            &rev,
            &unit,
            *kind,
            &format!("content for {label}"),
            Some(label),
        )
        .await;
        seed_occurrence(&state, &gen_id, &format!("{label}.txt"), &rev, &unit).await;
    }

    let outcome = materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");
    assert_eq!(
        outcome.occurrence_count, 5,
        "all 5 unit_kind values present"
    );

    let names = fts_names(&cache);
    for (_, label) in kinds {
        assert!(
            names.contains(label),
            "expected occurrence for unit_kind {label:?} in {names:?}"
        );
    }
}

// The `cache:fts_before_head` seam only exists inside `materialize_fts` when
// `local-rag-store` is built with the `failpoints` feature — without it,
// arming the failpoint is a no-op and the call always succeeds.
#[cfg(feature = "failpoints")]
#[tokio::test]
async fn fail_before_head_rolls_back_whole_tx() {
    let _serial = SERIAL.lock().await;
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 50).await;

    // Generation A materializes successfully.
    let gen_a = uuid(60);
    seed_generation(&state, &wt, &gen_a, 1).await;
    let rev_a = uuid(61);
    let unit_a = uuid(62);
    seed_file_content(
        &state,
        &rev_a,
        &unit_a,
        UnitKind::Symbol,
        "fn a() {}",
        Some("afn"),
    )
    .await;
    seed_occurrence(&state, &gen_a, "a.rs", &rev_a, &unit_a).await;
    let outcome_a = materialize_fts(&state, &cache, &wt, &gen_a, NOW)
        .await
        .expect("materialize A succeeds");

    // Generation B: armed failpoint fires just before the head write.
    let gen_b = uuid(70);
    seed_generation(&state, &wt, &gen_b, 2).await;
    let rev_b = uuid(71);
    let unit_b = uuid(72);
    seed_file_content(
        &state,
        &rev_b,
        &unit_b,
        UnitKind::Symbol,
        "fn b() {}",
        Some("bfn"),
    )
    .await;
    seed_occurrence(&state, &gen_b, "b.rs", &rev_b, &unit_b).await;

    arm("cache:fts_before_head");
    let result = materialize_fts(&state, &cache, &wt, &gen_b, NOW).await;
    disarm("cache:fts_before_head");
    assert!(result.is_err(), "armed failpoint must fail the call");

    // A's rows/head must still be exactly as they were — the whole B
    // transaction (including its staged deletes) rolled back.
    assert_eq!(fts_names(&cache), HashSet::from(["afn".to_string()]));
    assert_eq!(fts_row_count(&cache), 1);
    let head = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    assert_eq!(head.generation_id, gen_a);
    assert_eq!(head.manifest_hash, outcome_a.manifest_hash);

    // A clean retry (failpoint disarmed) now converges to B.
    let outcome_b = materialize_fts(&state, &cache, &wt, &gen_b, NOW)
        .await
        .expect("materialize B after disarm");
    assert_eq!(fts_names(&cache), HashSet::from(["bfn".to_string()]));
    let head = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    assert_eq!(head.generation_id, gen_b);
    assert_eq!(head.manifest_hash, outcome_b.manifest_hash);
}

#[tokio::test]
async fn no_valid_head_before_first_successful_build() {
    let _serial = SERIAL.lock().await;
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 80).await;
    let read = cache.open_read().expect("read conn");
    assert_eq!(
        read_fts_projection_head(&read, &wt).expect("read head"),
        None,
        "no fts_projection_head row before any successful materialize_fts call"
    );
}

#[tokio::test]
async fn evicted_normalized_text_is_recomputed_and_recached() {
    let _serial = SERIAL.lock().await;
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 90).await;
    let gen_id = uuid(95);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev = uuid(96);
    let unit = uuid(97);
    let blob_id = seed_file_content(
        &state,
        &rev,
        &unit,
        UnitKind::Symbol,
        "fn evicted() { 1 }",
        Some("evicted"),
    )
    .await;
    seed_occurrence(&state, &gen_id, "evicted.rs", &rev, &unit).await;

    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("first materialize (cold cache, recomputes)");
    let body_first = fts_body_for(&cache, "evicted");

    // Corrupt the cached normalized text directly (simulating cache damage).
    let (b,) = (blob_id.clone(),);
    cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE normalized_text_cache SET normalized_text = 'corrupted garbage' \
                 WHERE blob_id = ?1",
                rusqlite::params![b],
            )
        })
        .await
        .expect("corrupt cached text");

    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("second materialize (detects corruption, recomputes)");
    let body_second = fts_body_for(&cache, "evicted");
    assert_eq!(
        body_first, body_second,
        "recomputed body text must match the original, not the corrupted cache"
    );
    assert_ne!(body_second, "corrupted garbage");

    // The cache row itself must be healed, not left corrupted.
    let read = cache.open_read().expect("read conn");
    let healed: String = read
        .query_row(
            "SELECT normalized_text FROM normalized_text_cache WHERE blob_id = ?1",
            rusqlite::params![blob_id],
            |r| r.get(0),
        )
        .expect("read normalized_text_cache");
    assert_eq!(healed, body_second);
}

fn fts_body_for(cache: &CacheDb, name_token: &str) -> String {
    let read = cache.open_read().expect("read conn");
    read.query_row(
        "SELECT body FROM fts_occurrences WHERE name = ?1",
        rusqlite::params![name_token],
        |r| r.get(0),
    )
    .expect("read body")
}

#[tokio::test]
async fn repeated_build_is_byte_identical() {
    let _serial = SERIAL.lock().await;
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 100).await;
    let gen_id = uuid(105);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev = uuid(106);
    let unit = uuid(107);
    seed_file_content(
        &state,
        &rev,
        &unit,
        UnitKind::Symbol,
        "fn rep() {}",
        Some("rep"),
    )
    .await;
    seed_occurrence(&state, &gen_id, "rep.rs", &rev, &unit).await;

    let first = materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("first build");
    let rows_first = all_fts_rows(&cache);
    let head_first = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");

    let second = materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("second build");
    let rows_second = all_fts_rows(&cache);
    let head_second = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");

    assert_eq!(first, second, "outcome must be byte-identical on repeat");
    assert_eq!(
        rows_first, rows_second,
        "fts_occurrences content must be identical"
    );
    assert_eq!(
        head_first, head_second,
        "fts_projection_head must be identical (same now_ms)"
    );
}

/// Every `(name, qualified_name, path, signature, body)` tuple in
/// `fts_occurrences`, sorted for order-independent comparison.
fn all_fts_rows(cache: &CacheDb) -> Vec<(String, String, String, String, String)> {
    let read = cache.open_read().expect("read conn");
    let mut stmt = read
        .prepare(
            "SELECT name, qualified_name, path, signature, body FROM fts_occurrences ORDER BY name",
        )
        .expect("prepare");
    let mut rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect");
    rows.sort();
    rows
}
