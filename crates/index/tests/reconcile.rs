//! T05-03 acceptance tests for the generation builder (spec 06 §2).
//!
//! Each test builds a **real** worktree tree on disk under an isolated
//! [`TempHome`], scans it (T05-02), and runs [`build_generation`], asserting
//! structural sharing (A→B edit / rename / delete), the deferral of
//! unsupported-language files, skip-on-read, `projection_ready`-only-when-complete
//! with no activation, failure handling, and retry idempotence.
//!
//! Determinism: an isolated store + tree, a fixed `now_ms`, and a seeded
//! [`SeqUuidV7`] (a local `UuidSource` — `test-support` is deliberately
//! dependency-free, so the double lives here). No wall clock, no network.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use local_rag_core::identity::path::CaseSensitivity;
use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_index::classify::ClassifierConfig;
use local_rag_index::reconcile::{BuildError, BuildOutcome, build_generation};
use local_rag_index::scan::{ScanManifest, ScanMode, StatCache, scan, scan_paths};
use local_rag_store::registry::{
    GenerationState, WorktreeKind, create_repository, create_worktree, current_generation,
    generation_state,
};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    SkipReason, StateDb, active_generations, generation_accounted_paths, generation_file_count,
    generation_skip_tally,
};
use local_rag_test_support::TempHome;

/// A seeded, deterministic [`UuidSource`]: each call embeds a distinct
/// (monotone) millisecond timestamp, so ids are unique and reproducible without a
/// clock or entropy.
struct SeqUuidV7 {
    counter: AtomicU64,
}

impl SeqUuidV7 {
    fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }
}

impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(1000 + n, [0xAB; 10])
    }
}

/// An isolated store plus an on-disk worktree root and its registered id.
struct Fixture {
    _home: TempHome,
    db: StateDb,
    root: PathBuf,
    worktree_id: String,
}

async fn fixture() -> Fixture {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let root = home.join("wt");
    std::fs::create_dir_all(&root).expect("create root");

    // Register a repo + main worktree so the generation FK resolves.
    let repo = "018f0000-0000-7000-8000-0000000000r1".replace('r', "a");
    let worktree_id = "018f0000-0000-7000-8000-0000000000w1".replace('w', "b");
    let (r, w) = (repo.clone(), worktree_id.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)
        })
        .await
        .expect("seed repo + worktree");

    Fixture {
        _home: home,
        db,
        root,
        worktree_id,
    }
}

/// Write `contents` to `root/rel`, creating parent directories.
fn write(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parents");
    }
    std::fs::write(path, contents).expect("write file");
}

/// Scan `root` with the common test defaults.
fn scan_tree(root: &Path) -> ScanManifest {
    let mut cache = StatCache::new();
    scan(
        root,
        WorktreeKind::Main,
        CaseSensitivity::Sensitive,
        1 << 20,
        ScanMode::Strict,
        &[],
        &mut cache,
    )
    .expect("scan")
    .0
}

/// Build a generation from `manifest` with the seeded uuid source.
async fn build(fx: &Fixture, manifest: &ScanManifest, uuids: &SeqUuidV7) -> BuildOutcome {
    build_generation(
        &fx.db,
        &fx.worktree_id,
        &fx.root,
        manifest,
        &ClassifierConfig::new(1 << 20),
        &Scanner::new(),
        uuids,
        2000,
    )
    .await
    .expect("build_generation")
}

/// Scan the current tree and build a generation (fresh scan, given uuid source).
async fn scan_and_build(fx: &Fixture, uuids: &SeqUuidV7) -> BuildOutcome {
    let manifest = scan_tree(&fx.root);
    build(fx, &manifest, uuids).await
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

/// `normalized_path`s that are `generation_file` members of `generation_id`.
fn members(conn: &Connection, generation_id: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT normalized_path FROM generation_file WHERE generation_id = ?1 ORDER BY normalized_path")
        .expect("prepare");
    stmt.query_map([generation_id], |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

/// `(normalized_path, reason)` skip rows of `generation_id`.
fn skips(conn: &Connection, generation_id: &str) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare("SELECT normalized_path, reason FROM skipped_file WHERE generation_id = ?1 ORDER BY normalized_path")
        .expect("prepare");
    stmt.query_map([generation_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

/// `(occurrence_id, normalized_path, unit_id)` of `generation_id`.
fn occurrences(conn: &Connection, generation_id: &str) -> Vec<(String, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT occurrence_id, normalized_path, unit_id \
             FROM generation_unit_occurrence WHERE generation_id = ?1 \
             ORDER BY occurrence_id",
        )
        .expect("prepare");
    stmt.query_map([generation_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

#[tokio::test]
async fn editing_one_file_shares_untouched_units() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "b.rs", b"fn b() {}\n");

    let gen1 = scan_and_build(&fx, &uuids).await;
    assert_eq!(gen1.files_indexed, 2);
    assert_eq!(gen1.revisions_created, 2, "both files new content");

    let read = fx.db.open_read().expect("read");
    let revisions_after_1 = count(&read, "file_revision");
    let units_after_1 = count(&read, "parsed_unit");
    drop(read);

    // Edit only a.rs.
    write(&fx.root, "a.rs", b"fn a() { let x = 1; }\n");
    let gen2 = scan_and_build(&fx, &uuids).await;

    assert_eq!(gen2.files_indexed, 2);
    assert_eq!(
        gen2.revisions_reused, 1,
        "b.rs (unchanged) reuses its revision — structural sharing",
    );
    assert_eq!(gen2.revisions_created, 1, "a.rs (edited) is a new revision");

    let read = fx.db.open_read().expect("read");
    // Only a.rs's new revision was added; b.rs's revision + units were NOT
    // duplicated (spec 06 §2.1 [FIXED] gate).
    assert_eq!(
        count(&read, "file_revision"),
        revisions_after_1 + 1,
        "exactly one new file_revision (the edited a.rs)",
    );
    assert!(
        count(&read, "parsed_unit") > units_after_1,
        "a.rs's new units were added",
    );
}

#[tokio::test]
async fn rename_shares_content_but_changes_occurrences() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "a.rs", b"fn a() {}\n");

    let gen1 = scan_and_build(&fx, &uuids).await;
    let read = fx.db.open_read().expect("read");
    let occ1 = occurrences(&read, &gen1.generation_id);
    let revisions_after_1 = count(&read, "file_revision");
    drop(read);
    assert!(!occ1.is_empty());

    // Rename a.rs -> b.rs (identical bytes at a new path).
    std::fs::rename(fx.root.join("a.rs"), fx.root.join("b.rs")).expect("rename");
    let gen2 = scan_and_build(&fx, &uuids).await;
    assert_eq!(
        gen2.revisions_reused, 1,
        "content is shared across the rename"
    );

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        count(&read, "file_revision"),
        revisions_after_1,
        "rename adds no new file_revision (content unchanged)",
    );
    let occ2 = occurrences(&read, &gen2.generation_id);

    // Same underlying units, but new occurrence ids at the new path.
    let unit_ids_1: Vec<&String> = occ1.iter().map(|(_, _, u)| u).collect();
    let unit_ids_2: Vec<&String> = occ2.iter().map(|(_, _, u)| u).collect();
    assert_eq!(unit_ids_1, unit_ids_2, "same shared unit ids");
    assert!(occ1.iter().all(|(_, p, _)| p == "a.rs"));
    assert!(occ2.iter().all(|(_, p, _)| p == "b.rs"));
    let occ_ids_1: std::collections::BTreeSet<&String> = occ1.iter().map(|(o, _, _)| o).collect();
    let occ_ids_2: std::collections::BTreeSet<&String> = occ2.iter().map(|(o, _, _)| o).collect();
    assert!(
        occ_ids_1.is_disjoint(&occ_ids_2),
        "occurrence ids differ because the path changed",
    );
}

#[tokio::test]
async fn deleted_file_is_absent_from_next_generation() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "b.rs", b"fn b() {}\n");

    scan_and_build(&fx, &uuids).await;
    std::fs::remove_file(fx.root.join("b.rs")).expect("delete b");
    let gen2 = scan_and_build(&fx, &uuids).await;

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        members(&read, &gen2.generation_id),
        vec!["a.rs".to_string()],
        "deleted b.rs is absent from generation N+1",
    );
}

#[tokio::test]
async fn read_failure_marks_failed_and_leaves_prior_untouched() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "a.rs", b"fn a() {}\n");

    let gen1 = scan_and_build(&fx, &uuids).await;

    // Add a NEW file, scan (so it is a candidate with a content hash), then delete
    // it on disk. Its content was never ingested, so the builder must read it —
    // and the read now fails.
    write(&fx.root, "c.rs", b"fn c() {}\n");
    let manifest = scan_tree(&fx.root);
    std::fs::remove_file(fx.root.join("c.rs")).expect("delete c");

    let err: BuildError = build_generation(
        &fx.db,
        &fx.worktree_id,
        &fx.root,
        &manifest,
        &ClassifierConfig::new(1 << 20),
        &Scanner::new(),
        &uuids,
        2000,
    )
    .await
    .expect_err("read of deleted c.rs must fail the build");

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        generation_state(&read, &err.generation_id).expect("state"),
        Some(GenerationState::Failed),
        "the failed build's generation is `failed`",
    );
    assert_eq!(
        generation_state(&read, &gen1.generation_id).expect("state"),
        Some(GenerationState::ProjectionReady),
        "the prior generation is untouched",
    );
    assert!(
        active_generations(&read, &fx.worktree_id)
            .expect("active")
            .is_empty(),
        "the builder never activates (no active mutation)",
    );
}

#[tokio::test]
async fn retry_produces_no_duplicate_content() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "b.rs", b"fn b() {}\n");

    scan_and_build(&fx, &uuids).await;
    let read = fx.db.open_read().expect("read");
    let (rev1, blob1, unit1) = (
        count(&read, "file_revision"),
        count(&read, "content_blob"),
        count(&read, "parsed_unit"),
    );
    drop(read);

    // Rebuild the identical tree: a fresh generation, but all content is reused.
    let gen2 = scan_and_build(&fx, &uuids).await;
    assert_eq!(gen2.revisions_reused, 2, "both revisions reused on retry");

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        count(&read, "file_revision"),
        rev1,
        "no duplicate revisions"
    );
    assert_eq!(count(&read, "content_blob"), blob1, "no duplicate blobs");
    assert_eq!(count(&read, "parsed_unit"), unit1, "no duplicate units");
    assert_eq!(count(&read, "generation"), 2, "two generations exist");
}

#[tokio::test]
async fn complete_build_reaches_projection_ready_without_activating() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "a.rs", b"fn a() {}\n");

    let genr = scan_and_build(&fx, &uuids).await;

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        generation_state(&read, &genr.generation_id).expect("state"),
        Some(GenerationState::ProjectionReady),
        "a complete build reaches projection_ready",
    );
    assert!(
        active_generations(&read, &fx.worktree_id)
            .expect("active")
            .is_empty(),
        "T05-03 does not activate",
    );
    assert_eq!(
        current_generation(&read, &fx.worktree_id).expect("current"),
        None,
        "worktree.current_generation_id is untouched",
    );
}

/// D-098 inverts the old `unsupported_language_file_is_deferred`: a file whose
/// extension selects no v0 language is now **indexed** by the universal path, not
/// dropped without a record.
#[tokio::test]
async fn a_file_with_no_v0_language_is_indexed_universally() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "notes.md", b"# hello\n\nbody\n");
    write(&fx.root, "data.json", b"{\n  \"k\": 1\n}\n");

    let genr = scan_and_build(&fx, &uuids).await;
    assert_eq!(genr.files_indexed, 3, "all three are indexed");
    assert_eq!(genr.files_skipped, 0);

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        members(&read, &genr.generation_id),
        vec![
            "a.rs".to_string(),
            "data.json".to_string(),
            "notes.md".to_string(),
        ]
    );
    assert!(skips(&read, &genr.generation_id).is_empty());

    // Each landed under its own dialect, so identical bytes under two policies
    // stay two revisions — the property structural sharing keys on.
    let fingerprints = revision_fingerprints(&read, &genr.generation_id);
    assert_eq!(
        fingerprints.get("notes.md").map(String::as_str),
        Some("chunk=1;grammar=universal@1;lang=text;norm=1;queries=0")
    );
    assert_eq!(
        fingerprints.get("data.json").map(String::as_str),
        Some("chunk=1;grammar=universal@1;lang=config;norm=1;queries=0")
    );
    assert_eq!(
        fingerprints.get("a.rs").map(String::as_str),
        Some("chunk=1;grammar=tree-sitter-rust@1;lang=rust;norm=1;queries=1")
    );

    // And each produced real, searchable units — a file unit plus its sections.
    for path in ["a.rs", "notes.md", "data.json"] {
        assert!(
            occurrences(&read, &genr.generation_id)
                .iter()
                .any(|(_, p, _)| p == path),
            "{path} has no occurrence",
        );
    }
}

/// The invariant D-098 exists to establish: every file the scan produced is in
/// exactly one of the two membership tables. No third state, on any tree.
#[tokio::test]
async fn every_scanned_file_is_either_indexed_or_skipped() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    // One file per route the builder can take.
    write(&fx.root, "code.rs", b"fn a() {}\n"); // language
    write(&fx.root, "conf/values.yaml", b"image: x\nport: 8080\n"); // config
    write(&fx.root, "docs/readme.md", b"# Title\n\nprose\n"); // text
    write(&fx.root, "query.gql", b"query Q { a }\n"); // fallback
    write(&fx.root, "logo.svg", b"<svg></svg>\n"); // binary by extension
    write(&fx.root, "blob.rs", b"fn a() {}\0x"); // binary by content
    write(&fx.root, "creds.env", b"AWS=\"AKIAIOSFODNN7EXAMPLE\"\n"); // secret
    write(&fx.root, "empty.txt", b""); // empty, still a file

    let manifest = scan_tree(&fx.root);
    let genr = build(&fx, &manifest, &uuids).await;

    assert_eq!(
        genr.files_indexed + genr.files_skipped,
        manifest.entries.len(),
        "every scanned candidate is accounted for exactly once",
    );

    let read = fx.db.open_read().expect("read");
    let accounted = generation_accounted_paths(&read, &genr.generation_id).expect("accounted");
    let candidates: std::collections::BTreeSet<String> = manifest
        .entries
        .iter()
        .map(|e| e.normalized_path.clone())
        .collect();
    assert_eq!(
        accounted, candidates,
        "the two membership tables tile the scan's candidate set exactly",
    );

    // The refusals are refusals on purpose, each with its own reason.
    let by_path: std::collections::BTreeMap<String, String> =
        skips(&read, &genr.generation_id).into_iter().collect();
    assert_eq!(by_path.get("logo.svg").map(String::as_str), Some("binary"));
    assert_eq!(by_path.get("blob.rs").map(String::as_str), Some("binary"));
    assert_eq!(by_path.get("creds.env").map(String::as_str), Some("secret"));
    assert_eq!(by_path.len(), 3, "nothing else was refused: {by_path:?}");
}

#[tokio::test]
async fn supported_extension_binary_content_is_skipped() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    // A `.rs` file whose bytes contain a NUL → classified binary on read.
    write(&fx.root, "blob.rs", b"fn a() {}\0more");
    write(&fx.root, "ok.rs", b"fn ok() {}\n");

    let genr = scan_and_build(&fx, &uuids).await;
    assert_eq!(genr.files_indexed, 1, "ok.rs indexed");
    assert_eq!(genr.files_skipped, 1, "blob.rs skipped");

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        skips(&read, &genr.generation_id),
        vec![("blob.rs".to_string(), "binary".to_string())],
    );
    assert_eq!(
        members(&read, &genr.generation_id),
        vec!["ok.rs".to_string()]
    );
}

#[tokio::test]
async fn huge_file_is_skipped_without_reading() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "ok.rs", b"fn ok() {}\n");
    write(&fx.root, "big.rs", &[b'x'; 100]);

    // Cap below big.rs so the scan marks it huge (content_hash = None).
    let mut cache = StatCache::new();
    let manifest = scan(
        &fx.root,
        WorktreeKind::Main,
        CaseSensitivity::Sensitive,
        16,
        ScanMode::Strict,
        &[],
        &mut cache,
    )
    .expect("scan")
    .0;

    let genr = build_generation(
        &fx.db,
        &fx.worktree_id,
        &fx.root,
        &manifest,
        &ClassifierConfig::new(16),
        &Scanner::new(),
        &uuids,
        2000,
    )
    .await
    .expect("build");

    assert_eq!(genr.files_skipped, 1);
    let read = fx.db.open_read().expect("read");
    assert_eq!(
        skips(&read, &genr.generation_id),
        vec![("big.rs".to_string(), "huge".to_string())],
    );
}

// ---------------------------------------------------------------------
// T16-04: adversarial corpus, end-to-end through the real scan→classify→
// build pipeline (spec 12 §2/§5, 14 §6) — extends `classify.rs`'s own
// `every_skip_reason_yields_no_occurrence_and_no_source_blob`, which
// deliberately bypasses this seam by calling `classify()`/
// `insert_skipped_file()` directly, to the real driver a repo on disk
// actually goes through.
// ---------------------------------------------------------------------

/// spec 12 §2/§5 `[FIXED]`: a file whose content the redaction `Scanner`
/// flags as a secret is `skipped_file(reason='secret')` — no `source_blob`,
/// no occurrences — through the real pipeline, not just `classify()` called
/// directly (`adversarial.index.secret-content-skipped-end-to-end`).
#[tokio::test]
async fn secret_content_is_skipped_and_leaves_no_source_blob() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    // Same literal `crates/index/tests/classify.rs`'s own secret-skip test uses.
    write(&fx.root, "config.rs", b"aws = \"AKIAIOSFODNN7EXAMPLE\"\n");
    write(&fx.root, "ok.rs", b"fn ok() {}\n");

    let genr = scan_and_build(&fx, &uuids).await;
    assert_eq!(genr.files_indexed, 1, "ok.rs indexed");
    assert_eq!(genr.files_skipped, 1, "config.rs skipped");

    let read = fx.db.open_read().expect("read");
    assert_eq!(
        skips(&read, &genr.generation_id),
        vec![("config.rs".to_string(), "secret".to_string())]
    );
    assert_eq!(
        members(&read, &genr.generation_id),
        vec!["ok.rs".to_string()]
    );
}

/// spec 12 §2 threat model ("symlink/path tricks") + `crates/index/tests/
/// scan.rs::symlinks_are_excluded_and_not_followed`'s manifest-level
/// guarantee, now proven through the full pipeline: a symlink inside the
/// worktree pointing at a file *outside* the worktree root never becomes a
/// member or even a skip row (it is excluded before classification ever
/// sees it, per `scan()`'s own manifest) — only the real file it points at
/// (and whatever secret-shaped content lives there) never enters the store
/// at all (`adversarial.index.symlink-escape-excluded-end-to-end`).
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_escaping_the_worktree_root_produces_no_member_or_occurrence() {
    use std::os::unix::fs::symlink;

    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "ok.rs", b"fn ok() {}\n");
    let outside = fx._home.join("secret-outside.txt");
    std::fs::write(&outside, b"aws = \"AKIAIOSFODNN7EXAMPLE\"\n").expect("write outside file");
    symlink(&outside, fx.root.join("link.rs")).expect("symlink escaping the worktree root");

    let genr = scan_and_build(&fx, &uuids).await;
    assert_eq!(
        genr.files_indexed, 1,
        "only ok.rs — the symlink is invisible"
    );

    let read = fx.db.open_read().expect("read");
    assert!(
        skips(&read, &genr.generation_id).is_empty(),
        "no skip row either — the symlink is excluded, not classified"
    );
    assert_eq!(
        members(&read, &genr.generation_id),
        vec!["ok.rs".to_string()]
    );
}

// ---------------------------------------------------------------------------
// D-096 — the build's own tally, the durable rows, and the scan agree
// ---------------------------------------------------------------------------

/// The breakdown must be the same number the rows are, per reason — a tally
/// that is merely *near* the truth is worse than no tally, because it is
/// believed.
#[tokio::test]
async fn the_skip_breakdown_matches_the_rows_it_wrote() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "ok.rs", b"fn ok() {}\n");
    write(&fx.root, "blob.rs", b"fn a() {}\0more"); // binary
    write(
        &fx.root,
        "creds.rs",
        b"let aws = \"AKIAIOSFODNN7EXAMPLE\";\n",
    ); // secret
    write(
        &fx.root,
        "keys.rs",
        b"let gh = \"ghp_0123456789012345678901234567890123456\";\n",
    ); // secret

    let genr = scan_and_build(&fx, &uuids).await;
    assert_eq!(genr.files_indexed, 1);
    assert_eq!(genr.files_skipped, 3);
    assert_eq!(genr.skipped_by_reason.total(), genr.files_skipped);
    assert_eq!(genr.skipped_by_reason.get(SkipReason::Binary), 1);
    assert_eq!(genr.skipped_by_reason.get(SkipReason::Secret), 2);
    assert_eq!(genr.skipped_by_reason.get(SkipReason::Huge), 0);
    // Largest first, so the rendering leads with what actually dominates.
    // `secret` sorts *after* `binary` in `SkipReason::ALL`, so this string is
    // only reachable when the count is the sort key — the fixture is chosen to
    // tell the two orders apart rather than agree with both.
    assert_eq!(genr.skipped_by_reason.render(), "2 secret, 1 binary");

    // The durable read path must produce the identical tally: `project list`,
    // `doctor` and the daemon's cycle log all read it back rather than
    // remembering it, so a divergence here would be a lie in three commands.
    let read = fx.db.open_read().expect("read");
    let durable = generation_skip_tally(&read, &genr.generation_id).expect("tally");
    assert_eq!(durable, genr.skipped_by_reason);
    assert_eq!(
        generation_file_count(&read, &genr.generation_id).expect("count"),
        genr.files_indexed
    );
}

/// A `huge` file is skipped before its bytes are ever read, and it lands in the
/// same tally as the classified skips (it is written on a different branch of
/// the builder, which is exactly how a breakdown loses a category).
#[tokio::test]
async fn the_unread_huge_skip_is_in_the_breakdown_too() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "ok.rs", b"fn ok() {}\n");
    write(&fx.root, "big.rs", &[b'x'; 100]);

    let mut cache = StatCache::new();
    let manifest = scan(
        &fx.root,
        WorktreeKind::Main,
        CaseSensitivity::Sensitive,
        16,
        ScanMode::Strict,
        &[],
        &mut cache,
    )
    .expect("scan")
    .0;
    let genr = build(&fx, &manifest, &uuids).await;

    assert_eq!(genr.skipped_by_reason.get(SkipReason::Huge), 1);
    assert_eq!(genr.skipped_by_reason.total(), 1);
    let read = fx.db.open_read().expect("read");
    assert_eq!(
        generation_skip_tally(&read, &genr.generation_id).expect("tally"),
        genr.skipped_by_reason
    );
}

/// The accounted set is exactly indexed ∪ skipped, and after D-098 that set is
/// the whole tree — which is why `project coverage` now reports zero.
#[tokio::test]
async fn the_accounted_set_covers_the_whole_tree() {
    let fx = fixture().await;
    let uuids = SeqUuidV7::new();
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "blob.rs", b"fn a() {}\0more"); // skipped: binary
    write(&fx.root, "notes.md", b"# hello\n"); // universal: text
    write(&fx.root, "deploy/values.yaml", b"image: x\n"); // universal: config

    let genr = scan_and_build(&fx, &uuids).await;
    assert_eq!(genr.files_indexed, 3);
    assert_eq!(genr.files_skipped, 1);

    let read = fx.db.open_read().expect("read");
    let accounted = generation_accounted_paths(&read, &genr.generation_id).expect("accounted");
    assert_eq!(
        accounted,
        [
            "a.rs".to_string(),
            "blob.rs".to_string(),
            "deploy/values.yaml".to_string(),
            "notes.md".to_string(),
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>(),
        "the accounted set is the union of both membership tables, and covers the tree",
    );

    // The difference against the scan's own candidate list is the report.
    let candidates = scan_paths(
        &fx.root,
        WorktreeKind::Main,
        CaseSensitivity::Sensitive,
        &[],
    )
    .expect("scan_paths");
    let missing: Vec<&String> = candidates
        .iter()
        .filter(|p| !accounted.contains(*p))
        .collect();
    assert!(
        missing.is_empty(),
        "D-098's invariant: nothing the scan saw is unaccounted for, got {missing:?}",
    );
}

/// `revision_fingerprints`'s own contract, and the reason it exists: the dialect
/// a file was indexed under is readable back off the row.
fn revision_fingerprints(
    conn: &Connection,
    generation_id: &str,
) -> std::collections::BTreeMap<String, String> {
    let mut stmt = conn
        .prepare(
            "SELECT gf.normalized_path, fr.parser_fingerprint \
             FROM generation_file gf \
             JOIN file_revision fr ON fr.file_revision_id = gf.file_revision_id \
             WHERE gf.generation_id = ?1",
        )
        .expect("prepare");
    stmt.query_map([generation_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

/// `scan_paths` and `scan` must see the same candidates, because the coverage
/// report subtracts one from a set built by the other. Two walks that disagree
/// about gitignore or pruning would invent a gap or hide one — the pairwise
/// disagreement `D-089` already paid for once.
#[tokio::test]
async fn scan_paths_sees_exactly_what_the_manifest_sees() {
    let fx = fixture().await;
    write(&fx.root, "a.rs", b"fn a() {}\n");
    write(&fx.root, "docs/notes.md", b"# hi\n");
    write(&fx.root, ".hidden.rs", b"fn h() {}\n");
    write(&fx.root, "build/out.rs", b"fn o() {}\n");
    write(&fx.root, ".gitignore", b"build/\n");
    // `.git` must be pruned by both, not just by one of them.
    write(&fx.root, ".git/config", b"[core]\n");

    let manifest = scan_tree(&fx.root);
    let from_manifest: Vec<String> = manifest
        .entries
        .iter()
        .map(|e| e.normalized_path.clone())
        .collect();
    let from_paths = scan_paths(
        &fx.root,
        WorktreeKind::Main,
        CaseSensitivity::Sensitive,
        &[],
    )
    .expect("scan_paths");

    assert_eq!(from_paths, from_manifest);
    assert!(
        !from_paths.iter().any(|p| p.starts_with(".git/")),
        "`.git` is pruned: {from_paths:?}",
    );
    assert!(
        !from_paths.iter().any(|p| p.starts_with("build/")),
        "gitignore is honored: {from_paths:?}",
    );
    assert!(from_paths.contains(&".gitignore".to_string()));
}
