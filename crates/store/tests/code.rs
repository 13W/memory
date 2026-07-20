//! T03-01 acceptance tests for the code-storage schema (spec 03 §2.3–2.4, 06 §2,
//! 12 §5): the full DDL constraint suite over the version-3 migration.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms` literals,
//! and ids minted from [`uuidv7_from`] with fixed entropy (no `SystemUuidV7`, so
//! no wall clock or `/dev/urandom`). Writer operations run through
//! [`StateWriter::transaction`]; reads use [`StateDb::open_read`].
//!
//! Pure enum round-trips and the corrupt-read idiom live in the `code` module's
//! unit tests; these exercise the DB operations, their exact constraints, and the
//! structural source-blob invariant (an occurrence exists only on a member file).

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::code::{
    EdgeResolution, NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, NewResolvedEdge,
    NewUnresolvedReference, NewlineStyle, SkipReason, SourceCompression, UnitKind,
    file_revision_id_by_content_key, insert_content_blob, insert_file_revision,
    insert_generation_file, insert_occurrence, insert_parsed_unit, insert_resolved_edge,
    insert_skipped_file, insert_unresolved_reference, member_file_revision, skip_reason,
};
use local_rag_store::registry::{WorktreeKind, create_repository, create_worktree};
use local_rag_store::rusqlite::Error;
use local_rag_store::rusqlite::ffi::{
    SQLITE_CONSTRAINT_CHECK, SQLITE_CONSTRAINT_FOREIGNKEY, SQLITE_CONSTRAINT_NOTNULL,
    SQLITE_CONSTRAINT_UNIQUE,
};
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (runs the
/// production migration set, including the code migration v3).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string keyed by `seed`.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// The SQLite **extended** result code of a failed write, if it was a SQLite
/// failure — lets a test prove *which* constraint fired (CHECK vs FK vs UNIQUE).
fn extended_code(err: &WriteError) -> Option<i32> {
    match err {
        WriteError::Sqlite(Error::SqliteFailure(e, _)) => Some(e.extended_code),
        _ => None,
    }
}

/// Identities of a seeded generation (repo → worktree → generation).
struct Gen {
    generation_id: String,
}

/// Seed a repository, an `active` main worktree, and one `active` generation under
/// it in a single transaction. The generation builder is group 05, so the row is
/// inserted directly — here we only need a valid FK parent for membership rows.
async fn seed_generation(db: &StateDb, seed: u8) -> Gen {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(60));
    let genr = uuid(seed.wrapping_add(120));
    let (r, w, g) = (repo.clone(), wt, genr.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, 1000)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, 1000)?;
            tx.execute(
                "INSERT INTO generation \
                   (generation_id, worktree_id, generation_number, state, created_at) \
                 VALUES (?1, ?2, 1, 'active', 1000)",
                (&g, &w),
            )
            .map(|_| ())
        })
        .await
        .expect("seed generation");
    Gen {
        generation_id: genr,
    }
}

/// Insert a `file_revision` with default bytes and the given reuse key; returns
/// its id.
async fn seed_revision(db: &StateDb, seed: u8, content_hash: &str, fingerprint: &str) -> String {
    let id = uuid(seed);
    let (i, ch, fp) = (
        id.clone(),
        content_hash.to_string(),
        fingerprint.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &i,
                    content_hash: &ch,
                    parser_fingerprint: &fp,
                    source_blob: b"hello\n",
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: 6,
                },
                1000,
            )
        })
        .await
        .expect("seed revision");
    id
}

/// Insert a `content_blob` and a `parsed_unit` in that blob for `revision`;
/// returns the unit id.
async fn seed_unit(db: &StateDb, seed: u8, revision: &str) -> String {
    let blob = uuid(seed.wrapping_add(30));
    let unit = uuid(seed.wrapping_add(40));
    let (b, u, rev) = (blob, unit.clone(), revision.to_string());
    db.writer()
        .transaction(move |tx| {
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &b,
                    language: "rust",
                    algo_version: 1,
                    normalization_version: 1,
                },
                1000,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u,
                    file_revision_id: &rev,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:main",
                    blob_id: &b,
                    span_start: 0,
                    span_end: 6,
                    local_name: Some("main"),
                    kind: Some("fn"),
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect("seed unit");
    unit
}

/// Happy path: insert a revision, blob, unit, membership, and occurrence; read
/// them back. The exact bytes round-trip and the reuse-key/member lookups resolve.
#[tokio::test]
async fn happy_path_insert_and_read() {
    let (_home, db) = open_state();
    let genr = seed_generation(&db, 1).await;
    let rev = seed_revision(&db, 2, "ch-a", "lang=rust;grammar=rs@1").await;
    let unit = seed_unit(&db, 3, &rev).await;

    let (g, path, r, u) = (
        genr.generation_id.clone(),
        "src/main.rs".to_string(),
        rev.clone(),
        unit.clone(),
    );
    let occ = uuid(9);
    let occ0 = occ.clone();
    db.writer()
        .transaction(move |tx| {
            insert_generation_file(tx, &g, &path, "src/main.rs", &r)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ0,
                    generation_id: &g,
                    normalized_path: &path,
                    unit_id: &u,
                    qualified_name: Some("crate::main"),
                    context_hash: None,
                },
            )
        })
        .await
        .expect("member + occurrence");

    let read = db.open_read().expect("read conn");

    // Exact bytes round-trip (BLOB NOT NULL), size and compression as stored.
    let (blob, size, comp): (Vec<u8>, i64, String) = read
        .query_row(
            "SELECT source_blob, source_size, source_compression \
             FROM file_revision WHERE file_revision_id = ?1",
            [&rev],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read revision");
    assert_eq!(blob, b"hello\n");
    assert_eq!(size, 6);
    assert_eq!(comp, "none");

    // Reuse-key and membership lookups resolve; a missing key is a clean None.
    assert_eq!(
        file_revision_id_by_content_key(&read, "ch-a", "lang=rust;grammar=rs@1").expect("lookup"),
        Some(rev.clone()),
    );
    assert_eq!(
        file_revision_id_by_content_key(&read, "ch-a", "lang=cpp;grammar=cc@1").expect("lookup"),
        None,
    );
    assert_eq!(
        member_file_revision(&read, &genr.generation_id, "src/main.rs").expect("member"),
        Some(rev),
    );

    let occ_count: i64 = read
        .query_row(
            "SELECT count(*) FROM generation_unit_occurrence WHERE occurrence_id = ?1",
            [&occ],
            |r| r.get(0),
        )
        .expect("count occ");
    assert_eq!(occ_count, 1);
}

/// The revision reuse key is `(content_hash, parser_fingerprint)`: a duplicate
/// pair is a UNIQUE violation, but the *same* content under a *different* parser
/// fingerprint is a distinct revision (spec 03 §2.3.1: `.c` vs `.cpp`).
#[tokio::test]
async fn revision_reuse_key_is_content_hash_and_fingerprint() {
    let (_home, db) = open_state();
    let _first = seed_revision(&db, 1, "same-hash", "lang=c;grammar=c@1").await;

    // Same (content_hash, fingerprint), different id → UNIQUE violation.
    let dup = uuid(2);
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &dup,
                    content_hash: "same-hash",
                    parser_fingerprint: "lang=c;grammar=c@1",
                    source_blob: b"hello\n",
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: 6,
                },
                1000,
            )
        })
        .await
        .expect_err("duplicate reuse key must be rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_UNIQUE));

    // Same content_hash, different fingerprint → allowed (a distinct revision).
    let _other = seed_revision(&db, 3, "same-hash", "lang=cpp;grammar=cc@1").await;
    let read = db.open_read().expect("read conn");
    let n: i64 = read
        .query_row(
            "SELECT count(*) FROM file_revision WHERE content_hash = ?1",
            ["same-hash"],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(
        n, 2,
        "same bytes under different parser fingerprints are two revisions"
    );
}

/// `parsed_unit` spans are byte offsets with `span_end >= span_start` (CHECK); an
/// empty span (`==`) is legal, an inverted one is rejected.
#[tokio::test]
async fn parsed_unit_span_must_be_ordered() {
    let (_home, db) = open_state();
    let rev = seed_revision(&db, 1, "ch", "fp").await;

    // Empty span is legal.
    let (blob_ok, unit_ok, r_ok) = (uuid(2), uuid(3), rev.clone());
    db.writer()
        .transaction(move |tx| {
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &blob_ok,
                    language: "rust",
                    algo_version: 1,
                    normalization_version: 1,
                },
                1000,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &unit_ok,
                    file_revision_id: &r_ok,
                    unit_kind: UnitKind::File,
                    syntax_locator: "file",
                    blob_id: &blob_ok,
                    span_start: 5,
                    span_end: 5,
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect("empty span is legal");

    // Inverted span is a CHECK violation.
    let (blob_bad, unit_bad, r_bad) = (uuid(4), uuid(5), rev.clone());
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &blob_bad,
                    language: "rust",
                    algo_version: 1,
                    normalization_version: 1,
                },
                1000,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &unit_bad,
                    file_revision_id: &r_bad,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:x",
                    blob_id: &blob_bad,
                    span_start: 10,
                    span_end: 3,
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect_err("inverted span must be rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_CHECK));
}

/// `parsed_unit` foreign keys: an unknown `file_revision_id` or `blob_id` is
/// rejected, and the `(file_revision_id, unit_kind, syntax_locator, span_start,
/// span_end)` locator is unique.
#[tokio::test]
async fn parsed_unit_foreign_keys_and_unique_locator() {
    let (_home, db) = open_state();
    let rev = seed_revision(&db, 1, "ch", "fp").await;
    let blob = uuid(2);
    let b0 = blob.clone();
    db.writer()
        .transaction(move |tx| {
            insert_content_blob(
                tx,
                &NewContentBlob {
                    blob_id: &b0,
                    language: "rust",
                    algo_version: 1,
                    normalization_version: 1,
                },
                1000,
            )
        })
        .await
        .expect("blob");

    // Unknown blob_id → FK violation.
    let (u1, r1) = (uuid(3), rev.clone());
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u1,
                    file_revision_id: &r1,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:a",
                    blob_id: "no-such-blob",
                    span_start: 0,
                    span_end: 1,
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect_err("unknown blob_id rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));

    // Unknown file_revision_id → FK violation.
    let (u2, b2) = (uuid(4), blob.clone());
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u2,
                    file_revision_id: "no-such-rev",
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:a",
                    blob_id: &b2,
                    span_start: 0,
                    span_end: 1,
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect_err("unknown revision rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));

    // Insert a unit, then a second with the same locator tuple → UNIQUE.
    let (u3, r3, b3) = (uuid(5), rev.clone(), blob.clone());
    db.writer()
        .transaction(move |tx| {
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u3,
                    file_revision_id: &r3,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:dup",
                    blob_id: &b3,
                    span_start: 0,
                    span_end: 4,
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect("first unit");
    let (u4, r4, b4) = (uuid(6), rev.clone(), blob.clone());
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u4,
                    file_revision_id: &r4,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: "fn:dup",
                    blob_id: &b4,
                    span_start: 0,
                    span_end: 4,
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect_err("duplicate locator rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_UNIQUE));
}

/// `generation_file` foreign keys: an unknown `generation_id` or
/// `file_revision_id` is rejected.
#[tokio::test]
async fn generation_file_requires_generation_and_revision() {
    let (_home, db) = open_state();
    let genr = seed_generation(&db, 1).await;
    let rev = seed_revision(&db, 2, "ch", "fp").await;

    // Unknown generation → FK.
    let r0 = rev.clone();
    let err = db
        .writer()
        .transaction(move |tx| insert_generation_file(tx, "no-such-genr", "a.rs", "a.rs", &r0))
        .await
        .expect_err("unknown generation rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));

    // Unknown revision → FK.
    let g0 = genr.generation_id.clone();
    let err = db
        .writer()
        .transaction(move |tx| insert_generation_file(tx, &g0, "a.rs", "a.rs", "no-such-rev"))
        .await
        .expect_err("unknown revision rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));
}

/// The structural source-blob invariant (spec 12 §5): an occurrence can only exist
/// on a `generation_file` member (composite FK). A non-member path — including a
/// skipped one — cannot carry an occurrence, and the `(generation, path, unit)`
/// tuple is unique.
#[tokio::test]
async fn occurrence_requires_generation_file_member() {
    let (_home, db) = open_state();
    let genr = seed_generation(&db, 1).await;
    let rev = seed_revision(&db, 2, "ch", "fp").await;
    let unit = seed_unit(&db, 3, &rev).await;

    // Occurrence on a non-member path → FK (no member row for (genr, path)).
    let (g, u) = (genr.generation_id.clone(), unit.clone());
    let occ = uuid(4);
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ,
                    generation_id: &g,
                    normalized_path: "ghost.rs",
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect_err("occurrence without a member file rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));

    // Make the path a member, then the occurrence inserts; a duplicate
    // (genr, path, unit) is a UNIQUE violation.
    let (g, r, u) = (genr.generation_id.clone(), rev.clone(), unit.clone());
    let occ1 = uuid(5);
    db.writer()
        .transaction(move |tx| {
            insert_generation_file(tx, &g, "real.rs", "real.rs", &r)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ1,
                    generation_id: &g,
                    normalized_path: "real.rs",
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect("member + occurrence");

    let (g, u) = (genr.generation_id.clone(), unit.clone());
    let occ2 = uuid(6);
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ2,
                    generation_id: &g,
                    normalized_path: "real.rs",
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect_err("duplicate (genr, path, unit) rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_UNIQUE));

    // An unknown unit_id → FK.
    let g = genr.generation_id.clone();
    let occ3 = uuid(7);
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ3,
                    generation_id: &g,
                    normalized_path: "real.rs",
                    unit_id: "no-such-unit",
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect_err("unknown unit rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));
}

/// A skipped file records `(path, reason)` and — being a non-member — never gets an
/// occurrence (spec 06 §2.2, 12 §5). The typed insert round-trips through
/// [`skip_reason`]; an out-of-domain reason via raw SQL is a CHECK violation.
#[tokio::test]
async fn skipped_file_reason_and_no_occurrence() {
    let (_home, db) = open_state();
    let genr = seed_generation(&db, 1).await;
    let rev = seed_revision(&db, 2, "ch", "fp").await;
    let unit = seed_unit(&db, 3, &rev).await;

    // Typed insert for every reason, read back through `skip_reason`.
    let g = genr.generation_id.clone();
    db.writer()
        .transaction(move |tx| {
            insert_skipped_file(tx, &g, "big.bin", SkipReason::Binary, Some("h1"))?;
            insert_skipped_file(tx, &g, "vendor.lock", SkipReason::Ignored, None)?;
            insert_skipped_file(tx, &g, "creds.env", SkipReason::Secret, None)
        })
        .await
        .expect("skips");

    let read = db.open_read().expect("read conn");
    assert_eq!(
        skip_reason(&read, &genr.generation_id, "big.bin").expect("read"),
        Some(SkipReason::Binary),
    );
    assert_eq!(
        skip_reason(&read, &genr.generation_id, "creds.env").expect("read"),
        Some(SkipReason::Secret),
    );

    // A skipped path is not a member, so an occurrence on it is rejected (the skip
    // ⇒ no-occurrence invariant is structural, not merely a convention).
    let (g, u) = (genr.generation_id.clone(), unit.clone());
    let occ = uuid(8);
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ,
                    generation_id: &g,
                    normalized_path: "big.bin",
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect_err("occurrence on a skipped file rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));

    // A reason outside the CHECK domain (raw SQL) is rejected.
    let g = genr.generation_id.clone();
    let err = db
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO skipped_file (generation_id, normalized_path, reason, content_hash) \
                 VALUES (?1, 'weird.txt', 'vendored', NULL)",
                [&g],
            )
            .map(|_| ())
        })
        .await
        .expect_err("out-of-domain reason rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_CHECK));
}

/// `unresolved_reference` and `resolved_graph_edge` foreign keys and the
/// `resolution` CHECK domain (spec 03 §2.4).
#[tokio::test]
async fn references_and_edges_constraints() {
    let (_home, db) = open_state();
    let genr = seed_generation(&db, 1).await;
    let rev = seed_revision(&db, 2, "ch", "fp").await;
    let unit = seed_unit(&db, 3, &rev).await;

    // unresolved_reference requires an existing revision + source unit.
    let (r, u) = (rev.clone(), unit.clone());
    db.writer()
        .transaction(move |tx| {
            insert_unresolved_reference(
                tx,
                &NewUnresolvedReference {
                    file_revision_id: &r,
                    source_unit_id: &u,
                    reference_text: "foo::bar",
                    reference_kind: "call",
                },
            )
        })
        .await
        .expect("unresolved reference");

    let u = unit.clone();
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_unresolved_reference(
                tx,
                &NewUnresolvedReference {
                    file_revision_id: "no-such-rev",
                    source_unit_id: &u,
                    reference_text: "x",
                    reference_kind: "call",
                },
            )
        })
        .await
        .expect_err("unknown revision rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));

    // Two member occurrences to anchor an edge.
    let (g, r, u) = (genr.generation_id.clone(), rev.clone(), unit.clone());
    let (occ_a, occ_b) = (uuid(10), uuid(11));
    let (a0, b0) = (occ_a.clone(), occ_b.clone());
    db.writer()
        .transaction(move |tx| {
            insert_generation_file(tx, &g, "a.rs", "a.rs", &r)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &a0,
                    generation_id: &g,
                    normalized_path: "a.rs",
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )?;
            insert_generation_file(tx, &g, "b.rs", "b.rs", &r)?;
            // A second parsed unit is not needed: distinct occurrence on b.rs
            // reuses the same unit_id but a different path (unique tuple holds).
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &b0,
                    generation_id: &g,
                    normalized_path: "b.rs",
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect("two occurrences");

    // A valid edge inserts.
    let (g, a, b) = (genr.generation_id.clone(), occ_a.clone(), occ_b.clone());
    db.writer()
        .transaction(move |tx| {
            insert_resolved_edge(
                tx,
                &NewResolvedEdge {
                    generation_id: &g,
                    src_occurrence_id: &a,
                    dst_occurrence_id: &b,
                    edge_kind: "call_heuristic",
                    resolution: EdgeResolution::Heuristic,
                },
            )
        })
        .await
        .expect("valid edge");

    // An edge to an unknown occurrence → FK.
    let (g, a) = (genr.generation_id.clone(), occ_a.clone());
    let err = db
        .writer()
        .transaction(move |tx| {
            insert_resolved_edge(
                tx,
                &NewResolvedEdge {
                    generation_id: &g,
                    src_occurrence_id: &a,
                    dst_occurrence_id: "no-such-occ",
                    edge_kind: "call_heuristic",
                    resolution: EdgeResolution::Syntax,
                },
            )
        })
        .await
        .expect_err("unknown destination occurrence rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_FOREIGNKEY));

    // A resolution outside the CHECK domain (raw SQL) is rejected.
    let (g, a, b) = (genr.generation_id.clone(), occ_a.clone(), occ_b.clone());
    let err = db
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO resolved_graph_edge \
                   (generation_id, src_occurrence_id, dst_occurrence_id, edge_kind, resolution) \
                 VALUES (?1, ?2, ?3, 'import', 'guess')",
                (&g, &a, &b),
            )
            .map(|_| ())
        })
        .await
        .expect_err("out-of-domain resolution rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_CHECK));
}

/// Sanity: a raw connection can list the code tables — proves the migration ran in
/// this fixture too (guards against a silently-empty schema).
#[tokio::test]
async fn code_tables_exist_after_migration() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    for t in [
        "file_revision",
        "content_blob",
        "parsed_unit",
        "generation_file",
        "skipped_file",
        "generation_unit_occurrence",
        "unresolved_reference",
        "resolved_graph_edge",
    ] {
        let n: i64 = read
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                [t],
                |r| r.get(0),
            )
            .expect("query table");
        assert_eq!(n, 1, "code table {t} exists");
    }
    // Indexes too.
    for idx in [
        "occurrence_by_gen",
        "occurrence_by_unit",
        "unresolved_by_rev",
    ] {
        let n: i64 = read
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name = ?1",
                [idx],
                |r| r.get(0),
            )
            .expect("query index");
        assert_eq!(n, 1, "code index {idx} exists");
    }
}

/// G03 property: the source-blob invariant holds at whole-generation granularity
/// (spec 12 §5, 03 §2.3–2.4). After building a realistic generation — several member
/// files with occurrences plus a skipped, non-member file — every row in
/// `generation_unit_occurrence` resolves `occurrence → generation_file →
/// file_revision` to a non-null `source_blob`, and the skipped file contributes
/// none. The composite membership FK already forbids the negative at write time
/// (`occurrence_requires_generation_file_member`); this data-level sweep proves the
/// forward chain end to end and is non-vacuous (there is at least one occurrence).
#[tokio::test]
async fn occurrence_implies_member_file_with_non_null_source_blob() {
    let (_home, db) = open_state();
    let genr = seed_generation(&db, 1).await;
    let rev_a = seed_revision(&db, 2, "ch-a", "fp-a").await;
    let rev_b = seed_revision(&db, 3, "ch-b", "fp-b").await;
    let unit_a = seed_unit(&db, 4, &rev_a).await;
    let unit_b = seed_unit(&db, 5, &rev_b).await;

    let (g, ra, rb, ua, ub) = (
        genr.generation_id.clone(),
        rev_a.clone(),
        rev_b.clone(),
        unit_a.clone(),
        unit_b.clone(),
    );
    let (occ_a, occ_b) = (uuid(20), uuid(21));
    db.writer()
        .transaction(move |tx| {
            insert_generation_file(tx, &g, "src/a.rs", "src/a.rs", &ra)?;
            insert_generation_file(tx, &g, "src/b.rs", "src/b.rs", &rb)?;
            // A skipped, non-member file lives in the same generation.
            insert_skipped_file(tx, &g, "vendor.min.js", SkipReason::Ignored, None)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ_a,
                    generation_id: &g,
                    normalized_path: "src/a.rs",
                    unit_id: &ua,
                    qualified_name: Some("crate::a"),
                    context_hash: None,
                },
            )?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &occ_b,
                    generation_id: &g,
                    normalized_path: "src/b.rs",
                    unit_id: &ub,
                    qualified_name: Some("crate::b"),
                    context_hash: None,
                },
            )
        })
        .await
        .expect("members + occurrences + skip");

    let read = db.open_read().expect("read conn");

    // The sweep is not vacuous: there is at least one occurrence to check.
    let total: i64 = read
        .query_row("SELECT count(*) FROM generation_unit_occurrence", [], |r| {
            r.get(0)
        })
        .expect("count occurrences");
    assert_eq!(total, 2, "fixture seeded two occurrences");

    // occurrence ⇒ generation_file ⇒ file_revision.source_blob (non-null): count the
    // rows that break the chain; there must be none.
    let orphans: i64 = read
        .query_row(
            "SELECT count(*) \
             FROM generation_unit_occurrence o \
             LEFT JOIN generation_file gf \
               ON gf.generation_id = o.generation_id \
              AND gf.normalized_path = o.normalized_path \
             LEFT JOIN file_revision fr \
               ON fr.file_revision_id = gf.file_revision_id \
             WHERE gf.file_revision_id IS NULL \
                OR fr.file_revision_id IS NULL \
                OR fr.source_blob IS NULL",
            [],
            |r| r.get(0),
        )
        .expect("sweep occurrences");
    assert_eq!(
        orphans, 0,
        "every occurrence must resolve to a member file with a non-null source_blob",
    );

    // The reverse absence: the skipped file is present but carries no occurrence.
    let skip_occ: i64 = read
        .query_row(
            "SELECT count(*) FROM generation_unit_occurrence \
             WHERE generation_id = ?1 AND normalized_path = 'vendor.min.js'",
            [&genr.generation_id],
            |r| r.get(0),
        )
        .expect("count skip occurrences");
    assert_eq!(skip_occ, 0, "a skipped file has no occurrences");
}

/// `file_revision.source_blob` is `BLOB NOT NULL` (spec 03 §2.3, the physical seat of
/// the invariant): a raw insert with a null blob is rejected. Guards against a future
/// migration or code path quietly persisting a revision without its exact bytes.
#[tokio::test]
async fn source_blob_is_not_nullable() {
    let (_home, db) = open_state();
    let id = uuid(1);
    let err = db
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO file_revision \
                   (file_revision_id, content_hash, parser_fingerprint, source_blob, \
                    source_compression, source_encoding, newline_style, source_size, created_at) \
                 VALUES (?1, 'ch-null', 'fp-null', NULL, 'none', 'utf-8', 'lf', 0, 1000)",
                [&id],
            )
            .map(|_| ())
        })
        .await
        .expect_err("null source_blob must be rejected");
    assert_eq!(extended_code(&err), Some(SQLITE_CONSTRAINT_NOTNULL));
}
