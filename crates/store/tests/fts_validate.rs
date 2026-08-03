//! T08-03 acceptance tests: [`open_and_validate_fts`] detects a stale/missing/
//! corrupt `fts_projection_head` (spec 06 §4) and repairs it by calling T08-02's
//! `materialize_fts` again, or defers to a background job when the fresh
//! occurrence count exceeds [`FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD`].
//!
//! Fixture style mirrors `crates/store/tests/fts_materialize.rs`: every
//! `state.sqlite` row is built directly through `local-rag-store`'s own
//! low-level primitives, no `crates/index` involvement. `set_current_generation`
//! is called right after seeding a generation to actually activate it (T08-02's
//! own tests didn't need this — they passed `generation_id` straight to
//! `materialize_fts`; T08-03's orchestrator instead reads the worktree's active
//! generation itself via `registry::current_generation`).
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, no
//! network, no wall-clock sleeps.

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
    CacheDb, FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD, FtsAvailability, FtsCheckOutcome,
    FtsDivergence, FtsOpenOutcome, StateDb, ValidationDepth, check_fts, derive_content_blob,
    fts_doc_occurrence_ids, fts_manifest_hash, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, materialize_fts, occurrence_id,
    occurrence_ids_for_generation, open_and_validate_fts, read_fts_projection_head,
    requires_index_unavailable,
};
use local_rag_test_support::TempHome;

const STORE_UUID: &str = "44444444-4444-7444-8444-444444444444";
const NOW: i64 = 1_000_000;

// ---- helpers (mirrors crates/store/tests/fts_materialize.rs) ----------------

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

/// Seed `count` occurrences for `generation_id` in one transaction — sharing a
/// single `file_revision`/`content_blob` (all unit spans identical, so they
/// content-address to the same `blob_id`; structural sharing across many
/// distinct paths/units is normal) purely to make a large-`count` fixture
/// cheap to build.
async fn seed_bulk_occurrences(state: &StateDb, generation_id: &str, count: u64) {
    let file_revision_id = uuid(900);
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

// ---- tests --------------------------------------------------------------------

#[tokio::test]
async fn bootstrap_before_any_generation_activated_is_no_active_generation() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 1).await;

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW)
        .await
        .expect("open_and_validate_fts");
    assert_eq!(outcome, FtsOpenOutcome::NoActiveGeneration);
    assert_eq!(fts_doc_row_count(&cache), 0, "cache must not be touched");
}

#[tokio::test]
async fn valid_head_after_materialize_is_reported_valid_with_no_rewrite() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 10).await;
    let gen_id = uuid(20);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev = uuid(21);
    let unit = uuid(22);
    seed_file_content(&state, &rev, &unit, "fn a() {}", Some("a")).await;
    seed_occurrence(&state, &gen_id, "a.rs", &rev, &unit).await;
    activate_generation(&state, &wt, &gen_id).await;

    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");
    let before = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 500)
        .await
        .expect("open_and_validate_fts");
    assert_eq!(outcome, FtsOpenOutcome::Valid);

    let after = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    assert_eq!(before, after, "a Valid outcome must not rewrite the head");
}

#[tokio::test]
async fn strong_validation_passes_immediately_after_a_real_rebuild() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 30).await;
    let gen_id = uuid(40);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev = uuid(41);
    let unit = uuid(42);
    seed_file_content(&state, &rev, &unit, "fn b() {}", Some("b")).await;
    seed_occurrence(&state, &gen_id, "b.rs", &rev, &unit).await;
    activate_generation(&state, &wt, &gen_id).await;

    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    // Independently recompute the manifest straight from state.sqlite, not by
    // reading back `outcome.manifest_hash` — a genuine round-trip check.
    let ids = {
        let read = state.open_read().expect("read conn");
        occurrence_ids_for_generation(&read, &gen_id).expect("read occurrence ids")
    };
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let expected_manifest = fts_manifest_hash(&wt, &gen_id, &refs);

    let head = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    assert_eq!(head.manifest_hash, expected_manifest);

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW)
        .await
        .expect("open_and_validate_fts");
    assert_eq!(outcome, FtsOpenOutcome::Valid);
}

#[tokio::test]
async fn stale_generation_head_self_heals() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 50).await;

    let gen_a = uuid(60);
    seed_generation(&state, &wt, &gen_a, 1).await;
    let rev_a = uuid(61);
    let unit_a = uuid(62);
    seed_file_content(&state, &rev_a, &unit_a, "fn a() {}", Some("a")).await;
    seed_occurrence(&state, &gen_a, "a.rs", &rev_a, &unit_a).await;
    activate_generation(&state, &wt, &gen_a).await;
    materialize_fts(&state, &cache, &wt, &gen_a, NOW)
        .await
        .expect("materialize A");

    // Activate generation B without touching the cache — the head now points
    // at a superseded generation.
    let gen_b = uuid(70);
    seed_generation(&state, &wt, &gen_b, 2).await;
    let rev_b = uuid(71);
    let unit_b = uuid(72);
    seed_file_content(&state, &rev_b, &unit_b, "fn b() {}", Some("b")).await;
    seed_occurrence(&state, &gen_b, "b.rs", &rev_b, &unit_b).await;
    activate_generation(&state, &wt, &gen_b).await;

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Cheap, NOW + 100)
        .await
        .expect("open_and_validate_fts");
    match outcome {
        FtsOpenOutcome::Rebuilt(o) => assert_eq!(o.occurrence_count, 1),
        other => panic!("expected Rebuilt, got {other:?}"),
    }
    let head = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    assert_eq!(head.generation_id, gen_b);

    // Re-run: now valid, no further rebuild.
    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 200)
        .await
        .expect("open_and_validate_fts");
    assert_eq!(outcome, FtsOpenOutcome::Valid);
}

#[tokio::test]
async fn corrupted_manifest_with_matching_count_is_caught_only_by_strong_check() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 80).await;
    let gen_id = uuid(90);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev = uuid(91);
    let unit = uuid(92);
    seed_file_content(&state, &rev, &unit, "fn c() {}", Some("c")).await;
    seed_occurrence(&state, &gen_id, "c.rs", &rev, &unit).await;
    activate_generation(&state, &wt, &gen_id).await;
    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    // Corrupt the manifest hash directly; occurrence_count is left matching.
    let w = wt.clone();
    cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE fts_projection_head SET manifest_hash = 'corrupted-garbage' \
                 WHERE worktree_id = ?1",
                rusqlite::params![w],
            )
        })
        .await
        .expect("corrupt manifest");

    let cheap = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Cheap, NOW + 100)
        .await
        .expect("cheap validation");
    assert_eq!(
        cheap,
        FtsOpenOutcome::Valid,
        "cheap cannot see manifest corruption"
    );

    let strong = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 200)
        .await
        .expect("strong validation");
    match strong {
        FtsOpenOutcome::Rebuilt(_) => {}
        other => panic!("expected Rebuilt, got {other:?}"),
    }
    let head = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    assert_ne!(head.manifest_hash, "corrupted-garbage");
}

#[tokio::test]
async fn above_threshold_occurrence_count_defers_to_background() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 100).await;
    let gen_id = uuid(110);
    seed_generation(&state, &wt, &gen_id, 1).await;
    seed_bulk_occurrences(&state, &gen_id, FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD + 1).await;
    activate_generation(&state, &wt, &gen_id).await;
    // No materialize_fts call — the head stays missing (HeadMissing).

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Cheap, NOW)
        .await
        .expect("open_and_validate_fts");
    match outcome {
        FtsOpenOutcome::DeferredBackground {
            divergence,
            occurrence_count_estimate,
        } => {
            assert_eq!(divergence, FtsDivergence::HeadMissing);
            assert_eq!(
                occurrence_count_estimate,
                FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD + 1
            );
        }
        other => panic!("expected DeferredBackground, got {other:?}"),
    }
    assert_eq!(
        fts_doc_row_count(&cache),
        0,
        "no synchronous rebuild must have happened"
    );
}

/// D-006 regression: before the fix, `open_and_validate_fts` sourced its
/// "actual" occurrence count and manifest from `state.sqlite`'s immutable
/// per-generation expectation instead of `cache.sqlite`'s real content, so a
/// direct swap of one `fts_doc.occurrence_id` (row count unchanged) was
/// invisible to both the cheap and the strong check — exactly the
/// "equal-count, different ID set" corruption spec 06 §4's strong check
/// exists to catch.
#[tokio::test]
async fn strong_check_catches_swapped_occurrence_id_invisible_to_state_sqlite() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 140).await;
    let gen_id = uuid(150);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev_d = uuid(151);
    let unit_d = uuid(152);
    seed_file_content(&state, &rev_d, &unit_d, "fn d() {}", Some("d")).await;
    let occ_d = seed_occurrence(&state, &gen_id, "d.rs", &rev_d, &unit_d).await;
    let rev_e = uuid(153);
    let unit_e = uuid(154);
    seed_file_content(&state, &rev_e, &unit_e, "fn e() {}", Some("e")).await;
    let occ_e = seed_occurrence(&state, &gen_id, "e.rs", &rev_e, &unit_e).await;
    activate_generation(&state, &wt, &gen_id).await;

    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    // Swap one real occurrence_id for a fake one directly in fts_doc; the row
    // count and fts_projection_head are left untouched.
    let fake_id = "f".repeat(64);
    let (w, real_id, fake) = (wt.clone(), occ_d.clone(), fake_id.clone());
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
        "cheap check is deliberately blind to an equal-count id swap (spec 06 §4)"
    );

    let strong = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Strong, NOW + 200)
        .await
        .expect("strong validation");
    match strong {
        FtsOpenOutcome::Rebuilt(_) => {}
        other => panic!("expected Rebuilt, got {other:?}"),
    }

    let ids = {
        let read = cache.open_read().expect("read conn");
        fts_doc_occurrence_ids(&read, &wt).expect("read occurrence ids")
    };
    assert!(
        ids.contains(&occ_d) && ids.contains(&occ_e),
        "rebuild must restore the real occurrence ids: {ids:?}"
    );
    assert!(
        !ids.contains(&fake_id),
        "rebuild must remove the swapped-in fake id: {ids:?}"
    );
}

// ---------------------------------------------------------------------------
// T16-03: `check_fts` — the read-only half of `open_and_validate_fts`, pinned
// against the same fixtures to match its outcome, and proven never to mutate.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn check_fts_matches_bootstrap_no_active_generation() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 200).await;

    let state_read = state.open_read().expect("state read");
    let cache_read = cache.open_read().expect("cache read");
    let outcome =
        check_fts(&state_read, &cache_read, &wt, ValidationDepth::Strong).expect("check_fts");
    assert_eq!(outcome, FtsCheckOutcome::NoActiveGeneration);
}

#[tokio::test]
async fn check_fts_matches_valid_after_a_real_rebuild() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 210).await;
    let gen_id = uuid(220);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev = uuid(221);
    let unit = uuid(222);
    seed_file_content(&state, &rev, &unit, "fn g() {}", Some("g")).await;
    seed_occurrence(&state, &gen_id, "g.rs", &rev, &unit).await;
    activate_generation(&state, &wt, &gen_id).await;
    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    let state_read = state.open_read().expect("state read");
    let cache_read = cache.open_read().expect("cache read");
    let outcome =
        check_fts(&state_read, &cache_read, &wt, ValidationDepth::Strong).expect("check_fts");
    assert_eq!(outcome, FtsCheckOutcome::Valid);
}

#[tokio::test]
async fn check_fts_reports_the_same_generation_mismatch_open_and_validate_fts_would_repair() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 230).await;

    let gen_a = uuid(240);
    seed_generation(&state, &wt, &gen_a, 1).await;
    let rev_a = uuid(241);
    let unit_a = uuid(242);
    seed_file_content(&state, &rev_a, &unit_a, "fn a() {}", Some("a")).await;
    seed_occurrence(&state, &gen_a, "a.rs", &rev_a, &unit_a).await;
    activate_generation(&state, &wt, &gen_a).await;
    materialize_fts(&state, &cache, &wt, &gen_a, NOW)
        .await
        .expect("materialize A");

    // Activate generation B without touching the cache -- the head now points
    // at a superseded generation (same fixture as `stale_generation_head_self_heals`).
    let gen_b = uuid(250);
    seed_generation(&state, &wt, &gen_b, 2).await;
    let rev_b = uuid(251);
    let unit_b = uuid(252);
    seed_file_content(&state, &rev_b, &unit_b, "fn b() {}", Some("b")).await;
    seed_occurrence(&state, &gen_b, "b.rs", &rev_b, &unit_b).await;
    activate_generation(&state, &wt, &gen_b).await;

    let head_before = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    let rows_before = fts_doc_row_count(&cache);

    let state_read = state.open_read().expect("state read");
    let cache_read = cache.open_read().expect("cache read");
    let outcome =
        check_fts(&state_read, &cache_read, &wt, ValidationDepth::Cheap).expect("check_fts");
    match outcome {
        FtsCheckOutcome::Divergent {
            divergence,
            active_generation_id,
        } => {
            assert_eq!(active_generation_id, gen_b);
            assert_eq!(
                divergence,
                FtsDivergence::GenerationMismatch {
                    head: gen_a.clone(),
                    active: gen_b.clone(),
                }
            );
        }
        other => panic!("expected Divergent, got {other:?}"),
    }
    drop(state_read);
    drop(cache_read);

    // Read-only: check_fts alone never rebuilt anything, unlike
    // `open_and_validate_fts` on the identical fixture (proven above by
    // `stale_generation_head_self_heals`).
    let head_after = read_fts_projection_head(&cache.open_read().expect("read"), &wt)
        .expect("read head")
        .expect("head present");
    assert_eq!(
        head_before, head_after,
        "check_fts must not rewrite the head"
    );
    assert_eq!(
        rows_before,
        fts_doc_row_count(&cache),
        "check_fts must not touch fts_doc"
    );
}

#[tokio::test]
async fn check_fts_is_read_only_on_a_strong_only_divergence() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 260).await;
    let gen_id = uuid(270);
    seed_generation(&state, &wt, &gen_id, 1).await;
    let rev = uuid(271);
    let unit = uuid(272);
    seed_file_content(&state, &rev, &unit, "fn h() {}", Some("h")).await;
    let occ = seed_occurrence(&state, &gen_id, "h.rs", &rev, &unit).await;
    activate_generation(&state, &wt, &gen_id).await;
    materialize_fts(&state, &cache, &wt, &gen_id, NOW)
        .await
        .expect("materialize");

    // Swap the occurrence_id directly -- count-preserving corruption, the
    // same fixture as `strong_check_catches_swapped_occurrence_id_invisible_to_state_sqlite`.
    let fake_id = "f".repeat(64);
    let (w, real_id, fake) = (wt.clone(), occ.clone(), fake_id.clone());
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

    // Cheap is deliberately blind (count-preserving); check_fts must match.
    let state_read = state.open_read().expect("state read");
    let cache_read = cache.open_read().expect("cache read");
    let cheap = check_fts(&state_read, &cache_read, &wt, ValidationDepth::Cheap).expect("cheap");
    assert_eq!(cheap, FtsCheckOutcome::Valid);

    let strong = check_fts(&state_read, &cache_read, &wt, ValidationDepth::Strong).expect("strong");
    match strong {
        FtsCheckOutcome::Divergent { divergence, .. } => {
            assert_eq!(divergence, FtsDivergence::ManifestMismatch);
        }
        other => panic!("expected Divergent(ManifestMismatch), got {other:?}"),
    }
    drop(state_read);
    drop(cache_read);

    // Read-only: the swapped-in fake id is still there -- no rebuild happened.
    let ids = {
        let read = cache.open_read().expect("read conn");
        fts_doc_occurrence_ids(&read, &wt).expect("read occurrence ids")
    };
    assert!(
        ids.contains(&fake_id),
        "check_fts must not repair the swap: {ids:?}"
    );
}

#[tokio::test]
async fn no_dense_leg_with_deferred_fts_requires_index_unavailable() {
    let (_home, state, cache) = open_both();
    let wt = seed_worktree(&state, 120).await;
    let gen_id = uuid(130);
    seed_generation(&state, &wt, &gen_id, 1).await;
    seed_bulk_occurrences(&state, &gen_id, FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD + 1).await;
    activate_generation(&state, &wt, &gen_id).await;

    let outcome = open_and_validate_fts(&state, &cache, &wt, ValidationDepth::Cheap, NOW)
        .await
        .expect("open_and_validate_fts");
    let FtsOpenOutcome::DeferredBackground { divergence, .. } = outcome else {
        panic!("expected DeferredBackground, got {outcome:?}");
    };
    let availability = FtsAvailability::Unavailable(Some(divergence));
    assert!(requires_index_unavailable(&availability, false));
}
