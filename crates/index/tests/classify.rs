//! T03-02 integration: the classifier's skip decision reaches the store as a
//! `skipped_file` row, and such a path is structurally a non-member — it holds no
//! `source_blob` and can carry no occurrence (spec 06 §2.2, 12 §5).
//!
//! The pure classification rules are unit-tested inside `local-rag-index`; this
//! test wires `classify` to the real `state.sqlite` schema (via the store's async
//! bounded writer) to prove the seam. Deterministic: isolated [`TempHome`], fixed
//! `now_ms`, ids from [`uuidv7_from`] with fixed entropy.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_core::redaction::Scanner;
use local_rag_index::classify::{Classification, ClassifierConfig, GitignoreSetBuilder, classify};
use local_rag_store::code::{
    NewContentBlob, NewFileRevision, NewOccurrence, NewParsedUnit, NewlineStyle, SkipReason,
    SourceCompression, UnitKind, insert_content_blob, insert_file_revision, insert_occurrence,
    insert_parsed_unit, insert_skipped_file, member_file_revision, skip_reason,
};
use local_rag_store::registry::{WorktreeKind, create_repository, create_worktree};
use local_rag_store::rusqlite::Error;
use local_rag_store::rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY;
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (runs the
/// production migration set, including code migration v3).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 keyed by `seed`.
fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// The SQLite extended result code of a failed write, if it was a SQLite failure.
fn extended_code(err: &WriteError) -> Option<i32> {
    match err {
        WriteError::Sqlite(Error::SqliteFailure(e, _)) => Some(e.extended_code),
        _ => None,
    }
}

/// Seed repo → active main worktree → active generation; return the generation id.
async fn seed_generation(db: &StateDb, seed: u8) -> String {
    let repo = uuid(seed);
    let wt = uuid(seed.wrapping_add(60));
    let genr = uuid(seed.wrapping_add(120));
    let (r, w, g) = (repo, wt, genr.clone());
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
    genr
}

/// Seed a `file_revision` + `content_blob` + `parsed_unit`; return the unit id.
/// The unit is a valid FK target so a later occurrence failure is unambiguously
/// the *membership* foreign key, not a missing unit.
async fn seed_unit(db: &StateDb) -> String {
    let rev = uuid(10);
    let blob = uuid(11);
    let unit = uuid(12);
    let (r, b, u) = (rev, blob, unit.clone());
    db.writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &r,
                    content_hash: "ch",
                    parser_fingerprint: "fp",
                    source_blob: b"hello\n",
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: 6,
                },
                1000,
            )?;
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
                    file_revision_id: &r,
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

#[tokio::test]
async fn classified_skip_becomes_non_member_with_no_occurrence() {
    let (_home, db) = open_state();
    let generation_id = seed_generation(&db, 1).await;
    let unit = seed_unit(&db).await;

    // A gitignored path and a binary path — two different reasons through one seam.
    let mut b = GitignoreSetBuilder::new("/repo");
    b.add_gitignore(".", "*.log\n");
    let ignores = b.build().expect("gitignore");
    let cfg = ClassifierConfig::new(1024);
    let scanner = Scanner::new();

    let cases = [
        ("build/app.log", b"log text".as_slice(), SkipReason::Ignored),
        (
            "assets/logo.png",
            b"\x89PNG\r\n\x1a\n".as_slice(),
            SkipReason::Binary,
        ),
    ];

    for (path, content, expected) in cases {
        // 1. Classification yields the expected skip (never Indexed).
        let verdict = classify(
            path,
            content.len() as u64,
            content,
            &ignores,
            &cfg,
            &scanner,
        );
        assert_eq!(
            verdict,
            Classification::Skipped(expected),
            "classify({path}) reason",
        );

        // 2. Record the skip through the store, exactly as reconcile would; a skip
        //    never carries a source_blob (insert_skipped_file has no such column).
        let (g, p) = (generation_id.clone(), path.to_string());
        db.writer()
            .transaction(move |tx| insert_skipped_file(tx, &g, &p, expected, None))
            .await
            .expect("insert skip");

        // 3. The path is not a member (no file_revision bound to it).
        let read = db.open_read().expect("read conn");
        assert_eq!(
            member_file_revision(&read, &generation_id, path).expect("member read"),
            None,
            "{path} must not be a generation_file member",
        );
        assert_eq!(
            skip_reason(&read, &generation_id, path).expect("skip read"),
            Some(expected),
        );

        // 4. Attaching an occurrence to the skipped path fails the membership FK —
        //    the structural "skipped ⇒ no occurrence" invariant (spec 12 §5).
        let occ = uuid(200);
        let (g, p, u) = (generation_id.clone(), path.to_string(), unit.clone());
        let err = db
            .writer()
            .transaction(move |tx| {
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &occ,
                        generation_id: &g,
                        normalized_path: &p,
                        unit_id: &u,
                        qualified_name: None,
                        context_hash: None,
                    },
                )
            })
            .await
            .expect_err("occurrence on a skipped path must fail");
        assert_eq!(
            extended_code(&err),
            Some(SQLITE_CONSTRAINT_FOREIGNKEY),
            "{path}: expected membership FK violation, got {err:?}",
        );
    }
}

/// G03 "reverse absence" for **every** skip reason (spec 06 §2.2, 12 §5). The test
/// above proves the seam for two reasons; the gate requires that each of the six —
/// ignored, huge, lfs, binary, encoding, secret — becomes a `skipped_file` that is a
/// non-member (no `file_revision`, hence no `source_blob`) and rejects an occurrence
/// on the membership FK. One fixture per reason, reused from the classifier's unit
/// tests, is driven through the real store seam.
#[tokio::test]
async fn every_skip_reason_yields_no_occurrence_and_no_source_blob() {
    const CAP: u64 = 1024;
    let (_home, db) = open_state();
    let generation_id = seed_generation(&db, 1).await;
    let unit = seed_unit(&db).await;

    let mut b = GitignoreSetBuilder::new("/repo");
    b.add_gitignore(".", "*.log\n");
    let ignores = b.build().expect("gitignore");
    let cfg = ClassifierConfig::new(CAP);
    let scanner = Scanner::new();

    let lfs_pointer = "version https://git-lfs.github.com/spec/v1\n\
                       oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
                       size 12345\n";

    // (path, content, expected). `huge` is stat-only: its content is empty and its
    // size is forced one byte over the cap below.
    let cases = [
        ("build/app.log", b"log text".as_slice(), SkipReason::Ignored),
        ("big.rs", b"".as_slice(), SkipReason::Huge),
        ("assets/model.bin", lfs_pointer.as_bytes(), SkipReason::Lfs),
        ("data/x", b"ab\0cd".as_slice(), SkipReason::Binary),
        (
            "weird.txt",
            b"\xFF\xFE\x41".as_slice(),
            SkipReason::Encoding,
        ),
        (
            "config.py",
            b"aws = \"AKIAIOSFODNN7EXAMPLE\"\n".as_slice(),
            SkipReason::Secret,
        ),
    ];

    for (path, content, expected) in cases {
        let size = if expected == SkipReason::Huge {
            CAP + 1
        } else {
            content.len() as u64
        };

        // 1. Classification yields exactly this reason (never Indexed).
        assert_eq!(
            classify(path, size, content, &ignores, &cfg, &scanner),
            Classification::Skipped(expected),
            "classify({path})",
        );

        // 2. Record it as reconcile would; a skip never carries a source_blob
        //    (`insert_skipped_file` has no such column).
        let (g, p) = (generation_id.clone(), path.to_string());
        db.writer()
            .transaction(move |tx| insert_skipped_file(tx, &g, &p, expected, None))
            .await
            .expect("insert skip");

        // 3. The path is a non-member: no file_revision is bound to it, so no
        //    source_blob exists for it.
        let read = db.open_read().expect("read conn");
        assert_eq!(
            member_file_revision(&read, &generation_id, path).expect("member read"),
            None,
            "{path}: skipped ⇒ non-member (no source_blob)",
        );
        assert_eq!(
            skip_reason(&read, &generation_id, path).expect("skip read"),
            Some(expected),
        );

        // 4. An occurrence on the skipped path fails the membership FK — the
        //    structural "skipped ⇒ no occurrence" invariant, proven per reason.
        let occ = uuid(200);
        let (g, p, u) = (generation_id.clone(), path.to_string(), unit.clone());
        let err = db
            .writer()
            .transaction(move |tx| {
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &occ,
                        generation_id: &g,
                        normalized_path: &p,
                        unit_id: &u,
                        qualified_name: None,
                        context_hash: None,
                    },
                )
            })
            .await
            .expect_err("occurrence on a skipped path must fail");
        assert_eq!(
            extended_code(&err),
            Some(SQLITE_CONSTRAINT_FOREIGNKEY),
            "{path}: expected membership FK violation, got {err:?}",
        );
    }
}
