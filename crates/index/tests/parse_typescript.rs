//! T04-03 integration: the TypeScript adapter against the neutral parser fixtures
//! (spec 14 §1.1), the determinism gate (spec 14 §5), the fingerprint reconciliation
//! (spec 03 §2.3.1), and a searchable round-trip through the real `state.sqlite`.
//!
//! The pure derivation rules are unit-tested inside `local-rag-index`; here the
//! fixtures pin observable behavior (kind/span/anchor/parent/refs) and the store
//! seam proves every produced `unit_kind` becomes a queryable occurrence.
//! Deterministic: isolated [`TempHome`], fixed `now_ms`, ids from [`uuidv7_from`].

use serde::Deserialize;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_index::parse::{
    LanguageId, LanguageParser, SyntaxAnchor, SyntaxLocator, TypeScriptParser, parser_fingerprint,
};
use local_rag_store::StateDb;
use local_rag_store::code::{
    NewOccurrence, NewParsedUnit, UnitKind, create_or_reuse_content_blob,
    create_or_reuse_file_revision, derive_content_blob, insert_generation_file, insert_occurrence,
    insert_parsed_unit, prepare_source,
};
use local_rag_store::registry::{WorktreeKind, create_repository, create_worktree};
use local_rag_test_support::TempHome;
use local_rag_test_support::fixtures::read_fixture;

// ── Neutral fixture model (deserialization is the schema check) ──────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    family: String,
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    description: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    #[allow(dead_code)]
    title: String,
    status: String,
    #[allow(dead_code)]
    provenance: Provenance,
    language: String,
    #[allow(dead_code)]
    category: String,
    source: String,
    expected_units: Vec<ExpectedUnit>,
    expected_unresolved: Vec<ExpectedRef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Provenance {
    #[allow(dead_code)]
    source: String,
    #[serde(default, rename = "ref")]
    #[allow(dead_code)]
    reference: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ExpectedUnit {
    unit_kind: String,
    span_start: u32,
    span_end: u32,
    local_name: Option<String>,
    kind: Option<String>,
    anchor: String,
    parent: Option<usize>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ExpectedRef {
    source_unit: usize,
    reference_text: String,
    reference_kind: String,
}

fn load_index() -> Index {
    let raw = read_fixture("parser/index.json").expect("read parser fixtures");
    serde_json::from_str(&raw).expect("parser fixtures deserialize (schema check)")
}

fn anchor_str(anchor: &SyntaxAnchor) -> String {
    match anchor {
        SyntaxAnchor::Path(p) => format!("p:{p}"),
        SyntaxAnchor::LocalOrdinal(n) => format!("o:{n}"),
    }
}

fn actual_units(out: &local_rag_index::parse::ParseOutput) -> Vec<ExpectedUnit> {
    out.units
        .iter()
        .map(|u| ExpectedUnit {
            unit_kind: u.unit_kind.as_str().to_string(),
            span_start: u.span.start,
            span_end: u.span.end,
            local_name: u.local_name.clone(),
            kind: u.lang_kind.clone(),
            anchor: anchor_str(&u.anchor),
            parent: u.parent,
        })
        .collect()
}

fn actual_refs(out: &local_rag_index::parse::ParseOutput) -> Vec<ExpectedRef> {
    out.unresolved
        .iter()
        .map(|r| ExpectedRef {
            source_unit: r.source_unit,
            reference_text: r.reference_text.clone(),
            reference_kind: r.reference_kind.as_str().to_string(),
        })
        .collect()
}

#[test]
fn parser_fixtures_match_expected_units_and_refs() {
    let index = load_index();
    assert_eq!(index.family, "parser");
    let parser = TypeScriptParser::new();
    let mut checked = 0usize;
    for case in &index.cases {
        assert_eq!(case.language, "typescript", "{}", case.id);
        assert_eq!(case.status, "active", "{}", case.id);
        let out = parser.parse(case.source.as_bytes());
        assert_eq!(
            actual_units(&out),
            case.expected_units,
            "units for {}",
            case.id
        );
        assert_eq!(
            actual_refs(&out),
            case.expected_unresolved,
            "unresolved refs for {}",
            case.id
        );
        checked += 1;
    }
    assert!(checked >= 4, "expected the authored parser cases");
}

#[test]
fn parser_output_is_byte_identical_on_reparse() {
    // The determinism gate (spec 14 §5): same (content, parser_fingerprint) ⇒
    // byte-identical unit sets. We compare the neutral projection twice per case
    // and across a fresh parser instance.
    let index = load_index();
    for case in &index.cases {
        let a = TypeScriptParser::new().parse(case.source.as_bytes());
        let b = TypeScriptParser::new().parse(case.source.as_bytes());
        assert_eq!(a, b, "reparse differs for {}", case.id);
        assert_eq!(actual_units(&a), actual_units(&b));
        assert_eq!(actual_refs(&a), actual_refs(&b));
    }
}

#[test]
fn fingerprint_is_reconciled_to_at_one_after_linking_the_grammar() {
    // §4.2 of the plan / ADR-0002: linking the real grammar keeps the T04-02 golden
    // green (versions reconciled to @1, no persisted data yet).
    assert_eq!(
        parser_fingerprint(LanguageId::TypeScript),
        "chunk=1;grammar=tree-sitter-typescript@1;lang=typescript;norm=1;queries=1"
    );
}

// ── Searchable round-trip through the real store ─────────────────────────────────

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

fn uuid(seed: u16) -> String {
    let mut rand = [0u8; 10];
    rand[8] = (seed >> 8) as u8;
    rand[9] = seed as u8;
    uuidv7_from(1000, rand).to_string()
}

/// One persistable unit, precomputed off the writer thread (all fields owned).
struct UnitRow {
    unit_id: String,
    unit_kind: UnitKind,
    locator: String,
    derived: local_rag_store::code::DerivedContentBlob,
    blob_id: String,
    span_start: i64,
    span_end: i64,
    local_name: Option<String>,
    kind: Option<String>,
    parent_unit_id: Option<String>,
    occurrence_id: String,
}

#[tokio::test]
async fn every_produced_unit_kind_is_searchable() {
    // A source that yields all TypeScript unit kinds this adapter produces:
    // `file`, `symbol`, and `fallback_chunk` (from the malformed tail).
    let source = "export function ok() {}\nfunction @@@ bad(\n";
    let parser = TypeScriptParser::new();
    let out = parser.parse(source.as_bytes());

    // Sanity: the source exercises all three kinds (else the test is vacuous).
    let produced: std::collections::BTreeSet<&str> =
        out.units.iter().map(|u| u.unit_kind.as_str()).collect();
    assert!(produced.contains("file"));
    assert!(produced.contains("symbol"));
    assert!(produced.contains("fallback_chunk"));

    // Precompute ids and per-unit rows (pure; off the writer thread).
    let unit_ids: Vec<String> = (0..out.units.len()).map(|i| uuid(500 + i as u16)).collect();
    let fingerprint = parser.parser_fingerprint();
    let rev_id = uuid(10);
    let rows: Vec<UnitRow> = out
        .units
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let slice = &source[u.span.start as usize..u.span.end as usize];
            let derived = derive_content_blob("typescript", slice);
            let blob_id = derived.blob_id.clone();
            let locator =
                SyntaxLocator::from_draft(u.locator_draft(LanguageId::TypeScript), blob_id.clone())
                    .serialize();
            UnitRow {
                unit_id: unit_ids[i].clone(),
                unit_kind: u.unit_kind,
                locator,
                derived,
                blob_id,
                span_start: u.span.start as i64,
                span_end: u.span.end as i64,
                local_name: u.local_name.clone(),
                kind: u.lang_kind.clone(),
                parent_unit_id: u.parent.map(|p| unit_ids[p].clone()),
                occurrence_id: uuid(900 + i as u16),
            }
        })
        .collect();

    let (_home, db) = open_state();
    let prepared = prepare_source(source.as_bytes());
    let repo = uuid(1);
    let wt = uuid(2);
    let generation_id = uuid(3);
    let norm_path = "src/app.ts".to_string();
    let display_path = norm_path.clone();

    let (g_for_read, path_for_read) = (generation_id.clone(), norm_path.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo, None, 1000)?;
            create_worktree(tx, &wt, &repo, WorktreeKind::Main, 1000)?;
            tx.execute(
                "INSERT INTO generation \
                   (generation_id, worktree_id, generation_number, state, created_at) \
                 VALUES (?1, ?2, 1, 'active', 1000)",
                (&generation_id, &wt),
            )?;
            let rev = create_or_reuse_file_revision(tx, &prepared, &fingerprint, &rev_id, 1000)?;
            let rev_id = rev.id().to_string();
            insert_generation_file(tx, &generation_id, &norm_path, &display_path, &rev_id)?;
            for row in &rows {
                create_or_reuse_content_blob(tx, &row.derived, "typescript", 1000)?;
                insert_parsed_unit(
                    tx,
                    &NewParsedUnit {
                        unit_id: &row.unit_id,
                        file_revision_id: &rev_id,
                        unit_kind: row.unit_kind,
                        syntax_locator: &row.locator,
                        blob_id: &row.blob_id,
                        span_start: row.span_start,
                        span_end: row.span_end,
                        local_name: row.local_name.as_deref(),
                        kind: row.kind.as_deref(),
                        parent_unit_id: row.parent_unit_id.as_deref(),
                    },
                )?;
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &row.occurrence_id,
                        generation_id: &generation_id,
                        normalized_path: &norm_path,
                        unit_id: &row.unit_id,
                        qualified_name: row.local_name.as_deref(),
                        context_hash: None,
                    },
                )?;
            }
            Ok(())
        })
        .await
        .expect("persist parse output");

    // Read back: every produced unit_kind is reachable as an occurrence on the
    // member path — i.e. searchable (spec 06 §2.1 "all kinds are indexed").
    let read = db.open_read().expect("read conn");
    let mut stmt = read
        .prepare(
            "SELECT DISTINCT pu.unit_kind \
               FROM generation_unit_occurrence o \
               JOIN parsed_unit pu ON pu.unit_id = o.unit_id \
              WHERE o.generation_id = ?1 AND o.normalized_path = ?2 \
              ORDER BY pu.unit_kind",
        )
        .expect("prepare");
    let kinds: Vec<String> = stmt
        .query_map((&g_for_read, &path_for_read), |r| r.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        kinds,
        vec![
            "fallback_chunk".to_string(),
            "file".to_string(),
            "symbol".to_string()
        ],
        "every produced unit kind must be a searchable occurrence"
    );
}
