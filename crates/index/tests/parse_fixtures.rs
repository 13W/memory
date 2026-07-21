//! T04-03/T04-04 integration: the per-language adapters against the neutral parser
//! fixtures (spec 14 §1.1), the determinism gate (spec 14 §5), the fingerprint
//! reconciliation (spec 03 §2.3.1), the extension→revision consequence
//! (spec 03 §2.3.1 / 06 §2.1 `[FIXED]`), and a searchable round-trip through the
//! real `state.sqlite`.
//!
//! The pure derivation rules are unit-tested inside `local-rag-index`; here the
//! multi-language fixtures pin observable behavior (kind/span/anchor/parent/refs),
//! routed to the parser named by each case's `language`. The store seam proves
//! every produced `unit_kind` becomes a queryable occurrence, and that
//! byte-identical source under different-language extensions forms distinct
//! `file_revision` rows. Deterministic: isolated [`TempHome`], fixed `now_ms`,
//! ids from [`uuidv7_from`].

use std::collections::BTreeMap;

use serde::Deserialize;

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_index::parse::{
    JavaScriptParser, LanguageId, LanguageParser, RustParser, SyntaxAnchor, TypeScriptParser,
    parser_fingerprint, persist_parse_output,
};
use local_rag_store::StateDb;
use local_rag_store::code::{
    NewOccurrence, RevisionOutcome, create_or_reuse_file_revision, insert_generation_file,
    insert_occurrence, prepare_source,
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

/// The adapter for a fixture's declared `language`. The choice lives in data (the
/// fixture's `language` field), not in the test — mirroring ADR-0001.
fn parser_for(language: &str) -> Box<dyn LanguageParser> {
    match language {
        "typescript" => Box::new(TypeScriptParser::new()),
        "javascript" => Box::new(JavaScriptParser::new()),
        "rust" => Box::new(RustParser::new()),
        other => panic!("no parser for fixture language {other:?}"),
    }
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
    let mut per_language: BTreeMap<String, usize> = BTreeMap::new();
    for case in &index.cases {
        assert_eq!(case.status, "active", "{}", case.id);
        let parser = parser_for(&case.language);
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
        *per_language.entry(case.language.clone()).or_default() += 1;
    }
    // Every v0 language must be exercised (TypeScript T04-03, JavaScript T04-04,
    // Rust T04-05).
    for lang in ["typescript", "javascript", "rust"] {
        assert!(
            per_language.get(lang).copied().unwrap_or(0) >= 4,
            "expected the authored {lang} cases"
        );
    }
}

#[test]
fn parser_output_is_byte_identical_on_reparse() {
    // The determinism gate (spec 14 §5): same (content, parser_fingerprint) ⇒
    // byte-identical unit sets. We compare the neutral projection twice per case
    // and across a fresh parser instance, for every language.
    let index = load_index();
    for case in &index.cases {
        let a = parser_for(&case.language).parse(case.source.as_bytes());
        let b = parser_for(&case.language).parse(case.source.as_bytes());
        assert_eq!(a, b, "reparse differs for {}", case.id);
        assert_eq!(actual_units(&a), actual_units(&b));
        assert_eq!(actual_refs(&a), actual_refs(&b));
    }
}

#[test]
fn fingerprints_are_reconciled_after_linking_the_grammars() {
    // §4.2 of the plan / ADR-0002: linking a real grammar keeps the T04-02 goldens
    // green (versions reconciled to @1, no persisted data yet).
    assert_eq!(
        parser_fingerprint(LanguageId::TypeScript),
        "chunk=1;grammar=tree-sitter-typescript@1;lang=typescript;norm=1;queries=1"
    );
    assert_eq!(
        parser_fingerprint(LanguageId::JavaScript),
        "chunk=1;grammar=tree-sitter-javascript@1;lang=javascript;norm=1;queries=1"
    );
    assert_eq!(
        parser_fingerprint(LanguageId::Rust),
        "chunk=1;grammar=tree-sitter-rust@1;lang=rust;norm=1;queries=1"
    );
}

#[test]
fn every_language_has_a_distinct_fingerprint() {
    // Cross-language fingerprint case (T04-05): byte-identical source under a
    // different language never collides in `file_revision` because `lang=`/`grammar=`
    // differ. All three v0 languages must render pairwise-distinct fingerprints.
    let fps: Vec<String> = LanguageId::ALL
        .iter()
        .map(|&l| parser_fingerprint(l))
        .collect();
    let mut deduped = fps.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(
        deduped.len(),
        fps.len(),
        "every v0 language must have a distinct parser_fingerprint"
    );
}

// ── Extension → revision consequence (spec 03 §2.3.1 / 06 §2.1 [FIXED]) ───────────

#[tokio::test]
async fn ambiguous_extension_yields_distinct_file_revisions() {
    // Language is chosen by extension, so byte-identical source under different
    // extensions (`.ts` → typescript vs `.js` → javascript) has different
    // parser_fingerprints and therefore forms *distinct* file_revision rows
    // (UNIQUE(content_hash, parser_fingerprint)). This is the local analogue of the
    // spec's `.c` vs `.cpp` consequence. Re-inserting the same (bytes, fingerprint)
    // reuses the row.
    let bytes = b"export function foo(a) {}\n";
    let fp_ts = parser_fingerprint(LanguageId::TypeScript);
    let fp_js = parser_fingerprint(LanguageId::JavaScript);
    assert_ne!(fp_ts, fp_js, "the two fingerprints must differ by lang=");

    let id_ts = uuid(11);
    let id_js = uuid(12);
    let id_ts_again = uuid(13);

    let (_home, db) = open_state();
    let outcomes = db
        .writer()
        .transaction(move |tx| {
            let prepared = prepare_source(bytes);
            let ts = create_or_reuse_file_revision(tx, &prepared, &fp_ts, &id_ts, 1000)?;
            let js = create_or_reuse_file_revision(tx, &prepared, &fp_js, &id_js, 1000)?;
            // Same bytes + same fingerprint ⇒ reuse the TypeScript row.
            let ts_again =
                create_or_reuse_file_revision(tx, &prepared, &fp_ts, &id_ts_again, 1000)?;
            Ok((ts, js, ts_again))
        })
        .await
        .expect("create/reuse file revisions");

    let (ts, js, ts_again) = outcomes;
    assert!(ts.is_created(), "the TypeScript revision is new");
    assert!(js.is_created(), "the JavaScript revision is new");
    assert_ne!(
        ts.id(),
        js.id(),
        "identical bytes under different extensions must be different revisions"
    );
    assert_eq!(
        ts_again,
        RevisionOutcome::Reused(ts.id().to_string()),
        "same bytes + same fingerprint must reuse the existing revision"
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

    // The content side goes through the T04-06 orchestrator; occurrences (group 05)
    // are then keyed off the unit ids it returns.
    let candidate_ids: Vec<String> = (0..out.units.len()).map(|i| uuid(500 + i as u16)).collect();
    let occ_ids: Vec<String> = (0..out.units.len()).map(|i| uuid(900 + i as u16)).collect();
    let fingerprint = parser.parser_fingerprint();
    let rev_id = uuid(10);

    let (_home, db) = open_state();
    let prepared = prepare_source(source.as_bytes());
    let repo = uuid(1);
    let wt = uuid(2);
    let generation_id = uuid(3);
    let norm_path = "src/app.ts".to_string();
    let display_path = norm_path.clone();
    let src_bytes = source.as_bytes().to_vec();
    let out_c = out.clone();

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
            let persisted = persist_parse_output(
                tx,
                &rev_id,
                LanguageId::TypeScript,
                &src_bytes,
                &out_c,
                &candidate_ids,
                1000,
            )?;
            for (i, unit) in out_c.units.iter().enumerate() {
                insert_occurrence(
                    tx,
                    &NewOccurrence {
                        occurrence_id: &occ_ids[i],
                        generation_id: &generation_id,
                        normalized_path: &norm_path,
                        unit_id: &persisted.unit_ids[i],
                        qualified_name: unit.local_name.as_deref(),
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
