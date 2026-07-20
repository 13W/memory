//! T03-03 acceptance tests: exact `source_blob` round-trip and `file_revision`
//! create-or-reuse (spec 03 §1.2, §2.3; 06 §2; 12 §5).
//!
//! Pure detection/compression round-trips live in the `code::source` module's
//! unit tests; these exercise the DB-facing path end to end — `prepare_source` →
//! `create_or_reuse_file_revision` (through [`StateWriter::transaction`]) →
//! `source_bytes` (through [`StateDb::open_read`]) — proving the stored bytes are
//! byte-identical to the input, that reuse is by `(content_hash,
//! parser_fingerprint)`, and that everything is idempotent on retry.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms` literals,
//! and ids minted from [`uuidv7_from`] with fixed entropy (no wall clock, no
//! `/dev/urandom`).

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::StateDb;
use local_rag_store::code::{
    NewlineStyle, PreparedSource, RevisionOutcome, create_or_reuse_file_revision, prepare_source,
    source_bytes,
};
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

/// Insert `prepared` under a fresh id + `fingerprint`, returning the outcome.
async fn ingest(
    db: &StateDb,
    prepared: PreparedSource,
    fingerprint: &str,
    new_id: &str,
) -> RevisionOutcome {
    let (p, f, i) = (prepared, fingerprint.to_string(), new_id.to_string());
    db.writer()
        .transaction(move |tx| create_or_reuse_file_revision(tx, &p, &f, &i, 1000))
        .await
        .expect("create-or-reuse")
}

/// Read the `(source_compression, source_size, newline_style)` triple stored for a
/// revision.
fn stored_meta(db: &StateDb, id: &str) -> (String, i64, String) {
    let read = db.open_read().expect("read conn");
    read.query_row(
        "SELECT source_compression, source_size, newline_style \
         FROM file_revision WHERE file_revision_id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .expect("read revision meta")
}

/// Every accepted-file shape round-trips byte-exactly, and `newline_style` /
/// `source_size` are recorded correctly.
#[tokio::test]
async fn roundtrip_lf_crlf_mixed_and_non_ascii() {
    let (_home, db) = open_state();

    let cases: &[(&[u8], NewlineStyle)] = &[
        (b"fn main() {}\n", NewlineStyle::Lf),
        (b"line1\r\nline2\r\n", NewlineStyle::Crlf),
        (b"a\r\nb\n", NewlineStyle::Mixed),
        (
            "héllo — café\n日本語のコード\n".as_bytes(),
            NewlineStyle::Lf,
        ),
    ];

    for (i, (raw, expected_nl)) in cases.iter().enumerate() {
        let id = uuid(i as u8);
        let prepared = prepare_source(raw);
        let outcome = ingest(&db, prepared, "lang=rust;grammar=rs@1", &id).await;
        assert_eq!(outcome, RevisionOutcome::Created(id.clone()));

        // Exact bytes reproduce, even through zstd.
        let read = db.open_read().expect("read conn");
        let bytes = source_bytes(&read, &id)
            .expect("source_bytes")
            .expect("row present");
        assert_eq!(bytes, *raw, "case {i} bytes must round-trip exactly");

        let (_comp, size, nl) = stored_meta(&db, &id);
        assert_eq!(
            size,
            raw.len() as i64,
            "case {i} size is uncompressed length"
        );
        assert_eq!(nl, expected_nl.as_str(), "case {i} newline_style");
    }
}

/// A compressible payload is stored as `zstd` (smaller blob) and an incompressible
/// one as `none`; both reproduce the exact original bytes from the DB.
#[tokio::test]
async fn compression_roundtrips_in_db() {
    let (_home, db) = open_state();

    // Highly compressible → zstd, and the stored blob is smaller than the source.
    let big = vec![b'z'; 8192];
    let big_id = uuid(1);
    ingest(
        &db,
        prepare_source(&big),
        "lang=text;grammar=txt@1",
        &big_id,
    )
    .await;
    let (comp, size, _nl) = stored_meta(&db, &big_id);
    assert_eq!(comp, "zstd");
    assert_eq!(size, 8192);
    let stored_len: i64 = {
        let read = db.open_read().expect("read conn");
        read.query_row(
            "SELECT length(source_blob) FROM file_revision WHERE file_revision_id = ?1",
            [&big_id],
            |r| r.get(0),
        )
        .expect("blob length")
    };
    assert!(
        stored_len < size,
        "zstd frame must be smaller than the source"
    );
    {
        let read = db.open_read().expect("read conn");
        assert_eq!(
            source_bytes(&read, &big_id)
                .expect("read")
                .expect("present"),
            big
        );
    }

    // Tiny/incompressible → none, still an exact round-trip.
    let small = b"x";
    let small_id = uuid(2);
    ingest(
        &db,
        prepare_source(small),
        "lang=text;grammar=txt@1",
        &small_id,
    )
    .await;
    let (comp, size, _nl) = stored_meta(&db, &small_id);
    assert_eq!(comp, "none");
    assert_eq!(size, 1);
    let read = db.open_read().expect("read conn");
    assert_eq!(
        source_bytes(&read, &small_id)
            .expect("read")
            .expect("present"),
        small
    );
}

/// The same bytes under the same fingerprint reuse the one row: the second call
/// returns `Reused` with the first id and inserts nothing.
#[tokio::test]
async fn same_key_reuses_row() {
    let (_home, db) = open_state();
    let raw = b"reused content\n";
    let fp = "lang=rust;grammar=rs@1";

    let first = ingest(&db, prepare_source(raw), fp, &uuid(1)).await;
    let first_id = match &first {
        RevisionOutcome::Created(id) => id.clone(),
        other => panic!("first ingest must create, got {other:?}"),
    };

    // A different new_id is offered but must be ignored on the reuse path.
    let second = ingest(&db, prepare_source(raw), fp, &uuid(2)).await;
    assert_eq!(second, RevisionOutcome::Reused(first_id.clone()));

    let read = db.open_read().expect("read conn");
    let n: i64 = read
        .query_row("SELECT count(*) FROM file_revision", [], |r| r.get(0))
        .expect("count");
    assert_eq!(n, 1, "reuse must not insert a second row");
}

/// The same bytes under a *different* parser fingerprint are two distinct
/// revisions (spec 03 §2.3.1: `.c` vs `.cpp`).
#[tokio::test]
async fn different_fingerprint_separates() {
    let (_home, db) = open_state();
    let raw = b"int main(void) { return 0; }\n";

    let c = ingest(&db, prepare_source(raw), "lang=c;grammar=c@1", &uuid(1)).await;
    let cpp = ingest(&db, prepare_source(raw), "lang=cpp;grammar=cc@1", &uuid(2)).await;
    assert!(c.is_created() && cpp.is_created());
    assert_ne!(c.id(), cpp.id(), "distinct fingerprints → distinct ids");

    let read = db.open_read().expect("read conn");
    // Same content_hash, two rows.
    let n: i64 = read
        .query_row(
            "SELECT count(DISTINCT parser_fingerprint) FROM file_revision \
             WHERE content_hash = (SELECT content_hash FROM file_revision LIMIT 1)",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(n, 2, "same bytes under two fingerprints are two revisions");
}

/// Mutating the caller's buffer after ingestion never changes the stored bytes —
/// `source_blob` is an independent copy.
#[tokio::test]
async fn live_file_mutation_does_not_affect_stored() {
    let (_home, db) = open_state();
    let original = b"stable original content\n".to_vec();

    let mut buf = original.clone();
    let prepared = prepare_source(&buf);
    let id = uuid(1);
    ingest(&db, prepared, "lang=rust;grammar=rs@1", &id).await;

    // Simulate the live file changing on disk after it was indexed.
    for byte in buf.iter_mut() {
        *byte = b'!';
    }
    buf.extend_from_slice(b"appended garbage");

    let read = db.open_read().expect("read conn");
    let stored = source_bytes(&read, &id).expect("read").expect("present");
    assert_eq!(
        stored, original,
        "stored bytes are independent of the live buffer"
    );
}

/// Replaying the whole logical operation is idempotent: one create then only
/// reuses, and the row count stays at one (state-changing retry rule).
#[tokio::test]
async fn reuse_is_idempotent_on_retry() {
    let (_home, db) = open_state();
    let raw = b"idempotent\n";
    let fp = "lang=rust;grammar=rs@1";

    let mut created = 0;
    let mut reused = 0;
    let mut canonical_id: Option<String> = None;
    for seed in 0..3u8 {
        let outcome = ingest(&db, prepare_source(raw), fp, &uuid(seed)).await;
        match outcome {
            RevisionOutcome::Created(id) => {
                created += 1;
                canonical_id = Some(id);
            }
            RevisionOutcome::Reused(id) => {
                reused += 1;
                assert_eq!(
                    Some(&id),
                    canonical_id.as_ref(),
                    "reuse returns the created id"
                );
            }
        }
    }
    assert_eq!(created, 1, "exactly one create across retries");
    assert_eq!(reused, 2, "the two replays reuse");

    let read = db.open_read().expect("read conn");
    let n: i64 = read
        .query_row("SELECT count(*) FROM file_revision", [], |r| r.get(0))
        .expect("count");
    assert_eq!(n, 1, "idempotent: no duplicate rows");
}
