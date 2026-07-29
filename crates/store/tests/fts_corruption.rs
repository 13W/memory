//! T08-04 acceptance tests: a corruption/staleness integration suite over
//! [`open_and_validate_fts`] (spec 06 §4; D-006 fixed the validation-input
//! source this suite exercises). Each scenario deletes/corrupts something
//! different — the head row, some `fts_doc`/`fts_occurrences` rows, one
//! `occurrence_id` swapped while the row count stays equal, or the whole
//! `cache.sqlite` file — and proves the result is either a full self-heal or
//! an explicit, cache-preserving degradation, never a silent pass-through.
//! Every scenario also asserts `state.sqlite` is untouched by the repair, and
//! a concurrent burst of validation calls is proven to converge to exactly
//! one correct row set, never duplicated rows.
//!
//! Fixture style mirrors `crates/store/tests/fts_validate.rs`: every
//! `state.sqlite` row is built directly through `local-rag-store`'s own
//! low-level primitives, no `crates/index` involvement. No FTS-specific JSON
//! fixture family exists (`fixtures/fault/matrix.json` only declares the
//! dense-projection `F` matrix, group 07, and group 13's `S` matrix) — this
//! suite is Rust-native named tests, matching T08-01's own scope precedent
//! (golden-token tables, not a fixture family).
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, no
//! network, no wall-clock sleeps. No test here arms the `cache:fts_before_head`
//! failpoint, so — unlike `fts_materialize.rs` — no cross-test `SERIAL` guard
//! is needed.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::code::{
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, UnitKind,
};
use local_rag_store::registry::{
    WorktreeKind, create_repository, create_worktree, set_current_generation,
};
use local_rag_store::rusqlite;
use local_rag_store::{
    CacheDb, CacheOpenOutcome, FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD, FtsDivergence,
    FtsOpenOutcome, StateDb, ValidationDepth, derive_content_blob, fts_doc_occurrence_ids,
    insert_content_blob, insert_file_revision, insert_generation_file, insert_occurrence,
    insert_parsed_unit, materialize_fts, occurrence_id, open_and_validate_fts,
};
use local_rag_test_support::TempHome;

const STORE_UUID: &str = "66666666-6666-7666-8666-666666666666";
const NOW: i64 = 1_000_000;

// ---- helpers (mirrors crates/store/tests/fts_validate.rs / fts_materialize.rs) ---

fn open_both() -> (TempHome, StateDb, CacheDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let cache = CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite");
    (home, state, cache)
}

fn uuid(seed: u16) -> String {
    let mut rand = [0u8; 10];
    rand[8] = (seed >> 8) as u8;
    rand[9] = (seed & 0xff) as u8;
    uuidv7_from(1000, rand).to_string()
}

async fn seed_worktree(state: &StateDb, seed: u16) -> String {
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

/// Point `worktree_id` at `generation_id` as its current generation —
/// `open_and_validate_fts` reads this, not `worktree_projection_state`.
async fn activate_generation(state: &StateDb, worktree_id: &str, generation_id: &str) {
    let (w, g) = (worktree_id.to_string(), generation_id.to_string());
    state
        .writer()
        .transaction(move |tx| set_current_generation(tx, &w, &g))
        .await
        .expect("activate generation");
}

async fn seed_file_content(
    state: &StateDb,
    file_revision_id: &str,
    unit_id: &str,
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
                    unit_kind: UnitKind::Symbol,
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

/// Seed `count` distinct occurrences for `generation_id` in one transaction —
/// sharing a single `file_revision`/`content_blob` purely to make a large
/// fixture cheap to build (mirrors `fts_validate.rs`'s own helper).
async fn seed_bulk_occurrences(state: &StateDb, generation_id: &str, count: u64) {
    let file_revision_id = uuid(999);
    let derived = derive_content_blob("rust", "a");
    let (gen_id, fr, blob, algo, norm) = (
        generation_id.to_string(),
        file_revision_id,
        derived.blob_id,
        derived.algo_version,
        derived.normalization_version,
    );
    state
        .writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &fr,
                    content_hash: &fr,
                    parser_fingerprint: "bulk-fp",
                    source_blob: b"a",
                    compression: local_rag_store::SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: local_rag_store::NewlineStyle::Lf,
                    source_size: 1,
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
            for i in 0..count {
                let unit_id = format!("bulk-unit-{i}");
                let path = format!("bulk{i}.rs");
                insert_parsed_unit(
                    tx,
                    &NewParsedUnit {
                        unit_id: &unit_id,
                        file_revision_id: &fr,
                        unit_kind: UnitKind::Symbol,
                        syntax_locator: &format!("loc:{i}"),
                        blob_id: &blob,
                        span_start: 0,
                        span_end: 1,
                        local_name: None,
                        kind: None,
                        parent_unit_id: None,
                    },
                )?;
                insert_generation_file(tx, &gen_id, &path, &path, &fr)?;
                let occ = occurrence_id(&gen_id, &path, &unit_id);
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &occ,
                        generation_id: &gen_id,
                        normalized_path: &path,
                        unit_id: &unit_id,
                        qualified_name: None,
                        context_hash: None,
                    },
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed bulk occurrences");
}

fn fts_doc_row_count(cache: &CacheDb) -> i64 {
    let read = cache.open_read().expect("read conn");
    read.query_row("SELECT COUNT(*) FROM fts_doc", [], |r| r.get(0))
        .expect("count")
}

/// A snapshot of the `state.sqlite` rows/counters `open_and_validate_fts`
/// must never mutate: the worktree's active generation pointer, how many
/// generations exist for it, and how many occurrences its generations own.
/// Equal before/after a repair proves the repair touched only `cache.sqlite`.
fn state_snapshot(state: &StateDb, worktree_id: &str) -> (Option<String>, i64, i64) {
    let read = state.open_read().expect("read conn");
    let current_generation_id: Option<String> = read
        .query_row(
            "SELECT current_generation_id FROM worktree WHERE worktree_id = ?1",
            rusqlite::params![worktree_id],
            |r| r.get(0),
        )
        .expect("read worktree");
    let generation_count: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM generation WHERE worktree_id = ?1",
            rusqlite::params![worktree_id],
            |r| r.get(0),
        )
        .expect("count generations");
    let occurrence_count: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM generation_unit_occurrence guo \
             JOIN generation g ON g.generation_id = guo.generation_id \
             WHERE g.worktree_id = ?1",
            rusqlite::params![worktree_id],
            |r| r.get(0),
        )
        .expect("count occurrences");
    (current_generation_id, generation_count, occurrence_count)
}

/// Append a suffix to a path's file name (`cache.sqlite` → `cache.sqlite-wal`).
fn append_suffix(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// Directly delete one occurrence's `fts_doc` + `fts_occurrences` row pair —
/// models corruption/data loss that leaves every other row (including the
/// stale `fts_projection_head`) untouched.
async fn delete_one_fts_row(cache: &CacheDb, worktree_id: &str, occurrence_id: &str) {
    let (w, o) = (worktree_id.to_string(), occurrence_id.to_string());
    cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "DELETE FROM fts_occurrences WHERE rowid IN \
                   (SELECT fts_rowid FROM fts_doc WHERE worktree_id = ?1 AND occurrence_id = ?2)",
                rusqlite::params![w, o],
            )?;
            tx.execute(
                "DELETE FROM fts_doc WHERE worktree_id = ?1 AND occurrence_id = ?2",
                rusqlite::params![w, o],
            )
        })
        .await
        .expect("delete one fts row");
}

// ---- tests --------------------------------------------------------------------

/// A previously valid head, deleted outright (crash/manual corruption), is
/// treated exactly like a fresh worktree that never had one: `HeadMissing`,
/// synchronous rebuild, `state.sqlite` untouched, and a second call confirms
/// convergence.
#[tokio::test]
async fn delete_head_row_is_rebuilt() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 10).await;
    let gen_id = uuid(20);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev = uuid(21);
    let unit = uuid(22);
    seed_file_content(&state, &rev, &unit, "fn f() {}", Some("f")).await;
    seed_occurrence(&state, &gen_id, "f.rs", &rev, &unit).await;
    activate_generation(&state, &wt, &gen_id).await;
    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    let before = state_snapshot(&state, &wt);

    let w = wt.clone();
    cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "DELETE FROM fts_projection_head WHERE worktree_id = ?1",
                rusqlite::params![w],
            )
        })
        .await
        .expect("delete head row");

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Cheap, NOW + 100)
        .await
        .expect("open_and_validate_fts");
    match outcome {
        FtsOpenOutcome::Rebuilt(o) => assert_eq!(o.occurrence_count, 1),
        other => panic!("expected Rebuilt, got {other:?}"),
    }
    assert_eq!(
        state_snapshot(&state, &wt),
        before,
        "state.sqlite must be untouched by the repair"
    );

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 200)
        .await
        .expect("open_and_validate_fts");
    assert_eq!(outcome, FtsOpenOutcome::Valid, "second call converges");
}

/// Deleting some (not all) `fts_doc`/`fts_occurrences` rows while leaving
/// `fts_projection_head` claiming the original count is caught by the cheap
/// count predicate alone (this is exactly the path D-006 fixed for the cheap
/// check) and fully restores the original occurrence set.
#[tokio::test]
async fn delete_some_fts_rows_is_rebuilt() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 30).await;
    let gen_id = uuid(40);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let mut expected_ids = Vec::new();
    for (i, name) in ["p", "q", "r"].iter().enumerate() {
        let base = 41 + (i as u16) * 2;
        let rev = uuid(base);
        let unit = uuid(base + 1);
        seed_file_content(
            &state,
            &rev,
            &unit,
            &format!("fn {name}() {{}}"),
            Some(name),
        )
        .await;
        let occ = seed_occurrence(&state, &gen_id, &format!("{name}.rs"), &rev, &unit).await;
        expected_ids.push(occ);
    }
    activate_generation(&state, &wt, &gen_id).await;
    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    let before = state_snapshot(&state, &wt);

    delete_one_fts_row(&cache, &wt, &expected_ids[0]).await;

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Cheap, NOW + 100)
        .await
        .expect("open_and_validate_fts");
    match outcome {
        FtsOpenOutcome::Rebuilt(o) => assert_eq!(o.occurrence_count, 3),
        other => panic!("expected Rebuilt, got {other:?}"),
    }

    let mut restored = {
        let read = cache.open_read().expect("read conn");
        fts_doc_occurrence_ids(&read, &wt).expect("read occurrence ids")
    };
    restored.sort();
    let mut expected = expected_ids.clone();
    expected.sort();
    assert_eq!(
        restored, expected,
        "rebuild must restore exactly the original occurrence set"
    );
    assert_eq!(
        state_snapshot(&state, &wt),
        before,
        "state.sqlite must be untouched by the repair"
    );
}

/// The literal "equal occurrence count, different ID set" corruption (D-006's
/// own regression is the same shape; this is T08-04's scenario-level version
/// with a 3-occurrence generation): the cheap check is deliberately blind
/// (spec 06 §4 — no manifest parameter exists), only the strong check catches
/// and repairs it.
#[tokio::test]
async fn swap_occurrence_id_keeps_count_equal_but_corrupts_manifest() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 50).await;
    let gen_id = uuid(60);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let mut expected_ids = Vec::new();
    for (i, name) in ["m", "n", "o"].iter().enumerate() {
        let base = 61 + (i as u16) * 2;
        let rev = uuid(base);
        let unit = uuid(base + 1);
        seed_file_content(
            &state,
            &rev,
            &unit,
            &format!("fn {name}() {{}}"),
            Some(name),
        )
        .await;
        let occ = seed_occurrence(&state, &gen_id, &format!("{name}.rs"), &rev, &unit).await;
        expected_ids.push(occ);
    }
    activate_generation(&state, &wt, &gen_id).await;
    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    let before = state_snapshot(&state, &wt);

    let fake_id = "e".repeat(64);
    let (w, real_id, fake) = (wt.clone(), expected_ids[0].clone(), fake_id.clone());
    cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE fts_doc SET occurrence_id = ?1 \
                 WHERE worktree_id = ?2 AND occurrence_id = ?3",
                rusqlite::params![fake, w, real_id],
            )
        })
        .await
        .expect("swap occurrence id");

    let cheap = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Cheap, NOW + 100)
        .await
        .expect("cheap validation");
    assert_eq!(
        cheap,
        FtsOpenOutcome::Valid,
        "cheap check has no manifest parameter and cannot see this"
    );

    let strong = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 200)
        .await
        .expect("strong validation");
    match strong {
        FtsOpenOutcome::Rebuilt(_) => {}
        other => panic!("expected Rebuilt, got {other:?}"),
    }

    let mut restored = {
        let read = cache.open_read().expect("read conn");
        fts_doc_occurrence_ids(&read, &wt).expect("read occurrence ids")
    };
    restored.sort();
    let mut expected = expected_ids.clone();
    expected.sort();
    assert_eq!(restored, expected, "rebuild must restore the real ids");
    assert!(!restored.contains(&fake_id));
    assert_eq!(
        state_snapshot(&state, &wt),
        before,
        "state.sqlite must be untouched by the repair"
    );
}

/// Losing the whole `cache.sqlite` file (and its `-wal`/`-shm` siblings) is
/// the most destructive corruption this suite models: on reopen there is no
/// schema at all until [`CacheDb::open`] recreates one, so validation must
/// still converge to the exact prior occurrence set from `state.sqlite` alone.
#[tokio::test]
async fn delete_whole_cache_file_is_fully_restored() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 70).await;
    let gen_id = uuid(80);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let mut expected_ids = Vec::new();
    for (i, name) in ["u", "v", "w"].iter().enumerate() {
        let base = 81 + (i as u16) * 2;
        let rev = uuid(base);
        let unit = uuid(base + 1);
        seed_file_content(
            &state,
            &rev,
            &unit,
            &format!("fn {name}() {{}}"),
            Some(name),
        )
        .await;
        let occ = seed_occurrence(&state, &gen_id, &format!("{name}.rs"), &rev, &unit).await;
        expected_ids.push(occ);
    }
    activate_generation(&state, &wt, &gen_id).await;
    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    let before = state_snapshot(&state, &wt);
    let cache_path = cache.path().to_path_buf();
    // `close()`, not `drop` (D-009 / D-022): `Drop` is asynchronous by design
    // (the writer thread is detached), so a bare `drop` here can race the
    // reopen below onto the same `-wal`/`-shm` sidecar names.
    cache.close();

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(append_suffix(&cache_path, suffix));
    }

    let cache = CacheDb::open(&cache_path, STORE_UUID).expect("reopen cache after total loss");
    assert_eq!(
        cache.outcome(),
        CacheOpenOutcome::Created,
        "the whole file was gone, so this must be a fresh create"
    );
    assert_eq!(fts_doc_row_count(&cache), 0, "the new cache starts empty");

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 1000)
        .await
        .expect("open_and_validate_fts");
    match outcome {
        FtsOpenOutcome::Rebuilt(o) => assert_eq!(o.occurrence_count, 3),
        other => panic!("expected Rebuilt, got {other:?}"),
    }

    let mut restored = {
        let read = cache.open_read().expect("read conn");
        fts_doc_occurrence_ids(&read, &wt).expect("read occurrence ids")
    };
    restored.sort();
    let mut expected = expected_ids.clone();
    expected.sort();
    assert_eq!(
        restored, expected,
        "total cache loss must still fully restore the exact occurrence set"
    );
    assert_eq!(
        state_snapshot(&state, &wt),
        before,
        "state.sqlite must be untouched by cache-file loss + rebuild"
    );
}

/// Three concurrent `open_and_validate_fts(Strong)` calls racing against the
/// same missing-head divergence must all resolve `Ok`, each either `Valid` or
/// `Rebuilt` (never an error), and converge to exactly one correct occurrence
/// set — never duplicated rows. `CacheWriter` is one dedicated OS thread
/// draining one bounded queue independently of the tokio runtime, so plain
/// `tokio::join!` on the runtime's default current-thread flavor already
/// exercises genuine cross-thread write serialization without needing
/// `Arc`+`spawn`+`multi_thread` (mirrors T07's projection concurrency tests).
#[tokio::test]
async fn concurrent_validation_rebuild_coalesces() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 90).await;
    let gen_id = uuid(100);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let mut expected_ids = Vec::new();
    for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
        let base = 101 + (i as u16) * 2;
        let rev = uuid(base);
        let unit = uuid(base + 1);
        seed_file_content(
            &state,
            &rev,
            &unit,
            &format!("fn {name}() {{}}"),
            Some(name),
        )
        .await;
        let occ = seed_occurrence(&state, &gen_id, &format!("{name}.rs"), &rev, &unit).await;
        expected_ids.push(occ);
    }
    activate_generation(&state, &wt, &gen_id).await;
    // No materialize_fts call — the head stays missing, forcing every
    // concurrent caller to race the same synchronous-rebuild decision.

    let (r1, r2, r3) = tokio::join!(
        open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW),
        open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 1),
        open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 2),
    );
    for r in [&r1, &r2, &r3] {
        match r {
            Ok(FtsOpenOutcome::Valid) | Ok(FtsOpenOutcome::Rebuilt(_)) => {}
            other => panic!("expected Ok(Valid | Rebuilt), got {other:?}"),
        }
    }

    let mut restored = {
        let read = cache.open_read().expect("read conn");
        fts_doc_occurrence_ids(&read, &wt).expect("read occurrence ids")
    };
    restored.sort();
    let mut expected = expected_ids.clone();
    expected.sort();
    assert_eq!(
        restored, expected,
        "concurrent rebuilds must converge to exactly one correct set, never duplicated rows"
    );
}

/// A divergence discovered on a generation above
/// [`FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD`] must defer to background without
/// touching the cache — the "explicitly degraded" half of the card. Unlike
/// T08-03's own above-threshold test (a bootstrap `HeadMissing` case that
/// never had a valid head at all), this seeds a real, previously-valid large
/// generation and then corrupts it, matching the card's "corrupts" framing.
#[tokio::test]
async fn corruption_above_threshold_defers_to_background_without_mutating_cache() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 110).await;
    let gen_id = uuid(120);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let total = FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD + 1;
    seed_bulk_occurrences(&state, &gen_id, total).await;
    activate_generation(&state, &wt, &gen_id).await;
    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");
    assert_eq!(fts_doc_row_count(&cache), total as i64);

    let before = state_snapshot(&state, &wt);

    let corrupted_id = {
        let read = cache.open_read().expect("read conn");
        fts_doc_occurrence_ids(&read, &wt).expect("read occurrence ids")[0].clone()
    };
    delete_one_fts_row(&cache, &wt, &corrupted_id).await;

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Cheap, NOW + 100)
        .await
        .expect("open_and_validate_fts");
    match outcome {
        FtsOpenOutcome::DeferredBackground {
            divergence,
            occurrence_count_estimate,
        } => {
            assert_eq!(
                divergence,
                FtsDivergence::OccurrenceCountMismatch {
                    head: total as i64,
                    actual: total as i64 - 1,
                }
            );
            assert_eq!(occurrence_count_estimate, total);
        }
        other => panic!("expected DeferredBackground, got {other:?}"),
    }

    assert_eq!(
        fts_doc_row_count(&cache),
        total as i64 - 1,
        "no synchronous rebuild must have happened; the cache stays corrupted"
    );
    assert_eq!(
        state_snapshot(&state, &wt),
        before,
        "state.sqlite must be untouched"
    );
}
