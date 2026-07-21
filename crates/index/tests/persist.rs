//! T04-06 integration: [`persist_parse_output`] against the real `state.sqlite`.
//!
//! Property/behavioral coverage of the content-side persistence: idempotence under
//! retry, no duplicate rows on the same revision, order-independence, content-blob
//! dedup, reference classification, atomic rollback (no partial graph), path-free
//! shared rows, and parent linkage. Deterministic: isolated [`TempHome`], fixed
//! `now_ms`, candidate unit ids from a seeded [`SeqUuids`] — no wall clock, sleep,
//! or `/dev/urandom`.

use local_rag_core::paths::StoreLayout;
use local_rag_index::parse::{
    LanguageId, LanguageParser, ParseOutput, ParsedUnitDraft, PersistOutcome, TypeScriptParser,
    persist_parse_output,
};
use local_rag_store::rusqlite::Connection;
use local_rag_store::{StateDb, UnitKind, create_or_reuse_file_revision, prepare_source};
use local_rag_test_support::{IdSource, SeqUuids, TempHome};

const NOW: i64 = 1000;
const FP: &str = "chunk=1;grammar=tree-sitter-typescript@1;lang=typescript;norm=1;queries=1";

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

fn parse(src: &str) -> ParseOutput {
    TypeScriptParser::new().parse(src.as_bytes())
}

/// Candidate unit ids for `output`, one per unit, from a seeded generator.
fn candidate_ids(output: &ParseOutput, seed: u64) -> Vec<String> {
    let id_gen = SeqUuids::seeded(seed);
    (0..output.units.len()).map(|_| id_gen.next_id()).collect()
}

/// Create/reuse the file revision for `source`, then persist `output` under it in
/// one transaction. Returns `(file_revision_id, outcome)`.
async fn persist(
    db: &StateDb,
    source: &str,
    output: &ParseOutput,
    id_seed: u64,
) -> (String, PersistOutcome) {
    let rev_id = SeqUuids::seeded(9000).next_id();
    let prepared = prepare_source(source.as_bytes());
    let ids = candidate_ids(output, id_seed);
    let (rid, fp, out, src, idv) = (
        rev_id.clone(),
        FP.to_string(),
        output.clone(),
        source.as_bytes().to_vec(),
        ids,
    );
    let outcome = db
        .writer()
        .transaction(move |tx| {
            let rev = create_or_reuse_file_revision(tx, &prepared, &fp, &rid, NOW)?;
            let rev_id = rev.id().to_string();
            persist_parse_output(tx, &rev_id, LanguageId::TypeScript, &src, &out, &idv, NOW)
        })
        .await
        .expect("persist parse output");
    (rev_id, outcome)
}

fn count(conn: &Connection, sql: &str, rev: &str) -> i64 {
    conn.query_row(sql, [rev], |r| r.get(0)).expect("count")
}

fn unit_count(conn: &Connection, rev: &str) -> i64 {
    count(
        conn,
        "SELECT count(*) FROM parsed_unit WHERE file_revision_id = ?1",
        rev,
    )
}

fn ref_count(conn: &Connection, rev: &str) -> i64 {
    count(
        conn,
        "SELECT count(*) FROM unresolved_reference WHERE file_revision_id = ?1",
        rev,
    )
}

fn blob_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM content_blob", [], |r| r.get(0))
        .expect("count blobs")
}

const NESTED: &str =
    "export function foo(a: number): void {}\nclass Host {\n  method(x: number) {}\n}\n";

#[tokio::test]
async fn persist_is_idempotent_under_retry_with_no_duplicates() {
    let (_home, db) = open_state();
    let out = parse(NESTED);
    let n = out.units.len();

    let (rev, first) = persist(&db, NESTED, &out, 1).await;
    assert_eq!(first.created_units, n, "first persist creates every unit");
    assert_eq!(first.reused_units, 0);

    // Re-persist the same revision — with *different* candidate ids to prove reuse
    // keys on the natural key, not the caller's ids.
    let (rev2, second) = persist(&db, NESTED, &out, 2).await;
    assert_eq!(rev2, rev, "same content+fingerprint reuses the revision");
    assert_eq!(second.created_units, 0, "retry creates nothing");
    assert_eq!(second.reused_units, n);
    assert_eq!(
        second.unit_ids, first.unit_ids,
        "reuse returns the original unit ids regardless of new candidates"
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(unit_count(&read, &rev), n as i64, "no duplicate units");
}

#[tokio::test]
async fn persisted_row_set_is_input_order_independent() {
    // Three top-level functions (all parent = None), so the symbol units can be
    // reordered without breaking parent indices. The engine precomputes each unit's
    // anchor/locator, so a reordered input must persist to the identical row set.
    let src = "function a(): void {}\nfunction b(): void {}\nfunction c(): void {}\n";
    let canonical = parse(src);

    // Build a variant with the symbol units reversed, file unit kept first.
    let mut units: Vec<ParsedUnitDraft> = canonical.units.clone();
    let file = units.remove(0);
    units.reverse();
    units.insert(0, file);
    let shuffled = ParseOutput {
        units,
        unresolved: canonical.unresolved.clone(),
    };

    let (_home_a, db_a) = open_state();
    let (rev_a, _) = persist(&db_a, src, &canonical, 1).await;
    let (_home_b, db_b) = open_state();
    let (rev_b, _) = persist(&db_b, src, &shuffled, 1).await;

    let keys = |db: &StateDb, rev: &str| -> Vec<(String, String, i64, i64)> {
        let conn = db.open_read().expect("read");
        let mut stmt = conn
            .prepare(
                "SELECT unit_kind, syntax_locator, span_start, span_end \
                   FROM parsed_unit WHERE file_revision_id = ?1 \
                  ORDER BY unit_kind, syntax_locator, span_start, span_end",
            )
            .expect("prepare");
        stmt.query_map([rev], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    };
    assert_eq!(
        keys(&db_a, &rev_a),
        keys(&db_b, &rev_b),
        "the persisted natural-key set must not depend on input order"
    );
}

#[tokio::test]
async fn content_blobs_are_deduped() {
    // Two functions with identical normalized bodies collapse to one shared blob
    // (plus the file blob and each symbol's own blob). The point: identical content
    // never creates a second content_blob row.
    let src = "function a(): void {}\nfunction b(): void {}\n";
    let out = parse(src);
    let (_home, db) = open_state();
    persist(&db, src, &out, 1).await;

    let read = db.open_read().expect("read");
    // Distinct blob_ids == total rows (PK) — assert no duplicate rows were inserted
    // for equal content by re-persisting and checking the count is unchanged.
    let before = blob_count(&read);
    persist(&db, src, &out, 2).await;
    let read2 = db.open_read().expect("read");
    assert_eq!(
        blob_count(&read2),
        before,
        "re-persist adds no content_blob rows"
    );
}

#[tokio::test]
async fn references_carry_file_source_and_kind() {
    let src = "import { A } from \"./mod\";\nimport type { T } from \"./types\";\nexport * from \"./re\";\n";
    let out = parse(src);
    let (_home, db) = open_state();
    let (rev, outcome) = persist(&db, src, &out, 1).await;

    let file_idx = out
        .units
        .iter()
        .position(|u| u.unit_kind == UnitKind::File)
        .expect("file unit");
    let file_unit_id = outcome.unit_ids[file_idx].clone();
    assert_eq!(outcome.references_inserted, 3);

    let read = db.open_read().expect("read");
    let mut stmt = read
        .prepare(
            "SELECT reference_text, reference_kind, source_unit_id \
               FROM unresolved_reference WHERE file_revision_id = ?1 \
              ORDER BY reference_text",
        )
        .expect("prepare");
    let rows: Vec<(String, String, String)> = stmt
        .query_map([&rev], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        rows,
        vec![
            (
                "./mod".to_string(),
                "import".to_string(),
                file_unit_id.clone()
            ),
            (
                "./re".to_string(),
                "reexport".to_string(),
                file_unit_id.clone()
            ),
            (
                "./types".to_string(),
                "type_import".to_string(),
                file_unit_id
            ),
        ]
    );

    // Idempotent: re-persist clears and reinserts to the same set (no growth).
    persist(&db, src, &out, 2).await;
    let read2 = db.open_read().expect("read");
    assert_eq!(
        ref_count(&read2, &rev),
        3,
        "refs are not duplicated on retry"
    );
}

#[tokio::test]
async fn rollback_leaves_no_partial_graph() {
    let (_home, db) = open_state();
    let out = parse(NESTED);
    let prepared = prepare_source(NESTED.as_bytes());
    let rev_id = SeqUuids::seeded(9000).next_id();

    // Create the revision in its own committed transaction so we can prove the
    // *persist* rolls back independently, leaving the revision but no graph.
    let (p, fp, rid) = (prepared, FP.to_string(), rev_id.clone());
    db.writer()
        .transaction(move |tx| create_or_reuse_file_revision(tx, &p, &fp, &rid, NOW).map(|_| ()))
        .await
        .expect("create revision");

    // A transaction that persists then deliberately errors → full rollback.
    let ids = candidate_ids(&out, 1);
    let (rid2, out2, idv) = (rev_id.clone(), out.clone(), ids);
    let result = db
        .writer()
        .transaction(move |tx| {
            persist_parse_output(
                tx,
                &rid2,
                LanguageId::TypeScript,
                NESTED.as_bytes(),
                &out2,
                &idv,
                NOW,
            )?;
            // Deliberate abort after a full persist: any Err rolls the tx back.
            Err::<(), _>(local_rag_store::rusqlite::Error::QueryReturnedNoRows)
        })
        .await;
    assert!(
        result.is_err(),
        "the deliberate abort must surface as an error"
    );

    let read = db.open_read().expect("read");
    assert_eq!(unit_count(&read, &rev_id), 0, "no partial parsed_unit rows");
    assert_eq!(ref_count(&read, &rev_id), 0, "no partial reference rows");
    assert_eq!(blob_count(&read), 0, "no partial content_blob rows");

    // A subsequent clean persist produces the full graph with no leftovers.
    let (_rev, outcome) = persist(&db, NESTED, &out, 2).await;
    assert_eq!(outcome.created_units, out.units.len());
    let read2 = db.open_read().expect("read");
    assert_eq!(unit_count(&read2, &rev_id), out.units.len() as i64);
}

#[tokio::test]
async fn parents_link_to_persisted_ids() {
    let (_home, db) = open_state();
    let out = parse(NESTED);
    let (rev, outcome) = persist(&db, NESTED, &out, 1).await;

    let read = db.open_read().expect("read");
    // Every unit whose draft has a parent index must store that parent's persisted
    // unit_id as its parent_unit_id.
    for (i, unit) in out.units.iter().enumerate() {
        let stored: Option<String> = read
            .query_row(
                "SELECT parent_unit_id FROM parsed_unit WHERE unit_id = ?1",
                [&outcome.unit_ids[i]],
                |r| r.get(0),
            )
            .expect("read parent");
        let expected = unit.parent.map(|p| outcome.unit_ids[p].clone());
        assert_eq!(stored, expected, "parent link for unit {i}");
    }
    // The method's parent is the class (a real nested link exists in this fixture).
    let method_idx = out
        .units
        .iter()
        .position(|u| u.local_name.as_deref() == Some("method"))
        .expect("method unit");
    assert!(out.units[method_idx].parent.is_some());
    let _ = rev;
}

#[tokio::test]
async fn shared_rows_have_no_path_or_context() {
    // spec 14 §5 schema audit: content-shared tables carry no path/generation column.
    let (_home, db) = open_state();
    let out = parse(NESTED);
    persist(&db, NESTED, &out, 1).await;

    let read = db.open_read().expect("read");
    let columns = |table: &str| -> Vec<String> {
        let mut stmt = read
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .expect("prepare");
        stmt.query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    };
    let forbidden = [
        "normalized_path",
        "display_path",
        "generation_id",
        "context_hash",
        "qualified_name",
    ];
    for table in ["content_blob", "parsed_unit", "unresolved_reference"] {
        let cols = columns(table);
        for bad in forbidden {
            assert!(
                !cols.iter().any(|c| c == bad),
                "{table} must not carry a path/context column ({bad})"
            );
        }
    }
}
