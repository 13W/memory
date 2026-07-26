//! T11-04 acceptance tests for the expected-subject computation shared by the
//! backfill worker and eviction (spec 10 §3, 06 §5, 03 §4.2).
//!
//! The worker itself lives in `local-rag-embed` (which depends on this crate, not
//! the other way round), so these cover the store-side half: which generations
//! count as pin roots, which model spaces are protected, how occurrences collapse
//! to subjects, and which required kinds have no subject function yet.
//!
//! Deterministic: isolated [`TempHome`], fixed `now_ms`, ids from `uuidv7_from`
//! with pinned entropy, no network, no sleeps.

use std::collections::BTreeSet;

use local_rag_core::identity::domain::subject_content_blob;
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::code::{NewFileRevision, NewOccurrence, NewParsedUnit, UnitKind};
use local_rag_store::registry::{
    DEFAULT_MODEL_SPACE_ID, DistanceMetric, ModelSpaceState, RepresentationKey, RepresentationKind,
    WorktreeKind, allocate_generation, create_model_space, create_repository, create_worktree,
    register_representation, set_model_space_representation, transition_generation,
    transition_model_space,
};
use local_rag_store::{
    DerivedContentBlob, EmbeddingKey, ExternalPins, GenerationState, RetentionParams, StateDb,
    SubjectKind, create_or_reuse_content_blob, derive_content_blob, expected_subject_keys,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    model_space_ids_in_states, occurrence_id, pinned_generations, protected_model_space_ids,
    protected_subject_keys,
};
use local_rag_test_support::TempHome;

const NOW: i64 = 1_000_000;
const REPRESENTATION_ID: &str = "77777777-7777-7777-8777-777777777777";
const LANGUAGE: &str = "rust";

fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

fn params() -> RetentionParams {
    RetentionParams {
        keep_last_k: 2,
        window_ms: 7 * 24 * 60 * 60 * 1000,
    }
}

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A worktree with one generation holding an occurrence per body; identical
/// bodies share one content blob by content addressing.
async fn seed_generation(db: &StateDb, worktree_seed: u8, bodies: &[&str]) -> (String, String) {
    let repo_id = uuid(worktree_seed);
    let worktree_id = uuid(worktree_seed + 1);
    let (r, w) = (repo_id.clone(), worktree_id.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, NOW)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, NOW)
        })
        .await
        .expect("seed repo + worktree");

    let generation_id = uuid(worktree_seed + 2);
    let (w, g) = (worktree_id.clone(), generation_id.clone());
    db.writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, NOW))
        .await
        .expect("allocate generation");

    for (i, body) in bodies.iter().enumerate() {
        let file_revision_id = uuid(worktree_seed + 20 + i as u8);
        let unit_id = uuid(worktree_seed + 40 + i as u8);
        let path = format!("src/{worktree_seed}_{i}.rs");
        let derived = derive_content_blob(LANGUAGE, body);
        let bytes = body.as_bytes().to_vec();
        let len = bytes.len() as i64;
        let (fr, u, d) = (
            file_revision_id.clone(),
            unit_id.clone(),
            derived.clone_for_tx(),
        );
        db.writer()
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
                create_or_reuse_content_blob(tx, &d, LANGUAGE, NOW)?;
                insert_parsed_unit(
                    tx,
                    &NewParsedUnit {
                        unit_id: &u,
                        file_revision_id: &fr,
                        unit_kind: UnitKind::Symbol,
                        syntax_locator: &format!("loc:{u}"),
                        blob_id: &d.blob_id,
                        span_start: 0,
                        span_end: len,
                        local_name: None,
                        kind: None,
                        parent_unit_id: None,
                    },
                )
            })
            .await
            .expect("seed unit");

        let occ = occurrence_id(&generation_id, &path, &unit_id);
        let (g, p, fr, u) = (
            generation_id.clone(),
            path.clone(),
            file_revision_id.clone(),
            unit_id.clone(),
        );
        db.writer()
            .transaction(move |tx| {
                insert_generation_file(tx, &g, &p, &p, &fr)?;
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
            .expect("seed occurrence");
    }

    let g = generation_id.clone();
    db.writer()
        .transaction(move |tx| transition_generation(tx, &g, GenerationState::ProjectionReady))
        .await
        .expect("transition")
        .expect("legal");

    (worktree_id, generation_id)
}

/// `DerivedContentBlob` is not `Clone` in the public API; tests only need a copy
/// to move into a transaction closure.
trait CloneForTx {
    fn clone_for_tx(&self) -> DerivedContentBlob;
}

impl CloneForTx for DerivedContentBlob {
    fn clone_for_tx(&self) -> DerivedContentBlob {
        DerivedContentBlob {
            blob_id: self.blob_id.clone(),
            normalized_text: self.normalized_text.clone(),
            byte_size: self.byte_size,
            algo_version: self.algo_version,
            normalization_version: self.normalization_version,
        }
    }
}

/// Register `kind` as a `required` representation of the default model space.
async fn require_kind(db: &StateDb, representation_id: &str, kind: RepresentationKind) -> String {
    let (id, key) = (
        representation_id.to_string(),
        RepresentationKey {
            kind,
            representation_version: 1,
            normalization_version: 1,
            model_id: "test-model".to_string(),
            dimensions: 3,
            distance_metric: DistanceMetric::Cosine,
        },
    );
    db.writer()
        .transaction(move |tx| {
            let registered = register_representation(tx, &id, &key, NOW)?;
            set_model_space_representation(
                tx,
                DEFAULT_MODEL_SPACE_ID,
                kind,
                &registered,
                true,
                NOW,
            )?;
            Ok(registered)
        })
        .await
        .expect("require kind")
}

/// Occurrences of one content blob collapse to a single subject, and the subject
/// hash is the domain-separated one (spec 03 §1.2/§4.2 `[FIXED]`).
#[tokio::test(flavor = "multi_thread")]
async fn occurrences_collapse_to_distinct_content_subjects() {
    let (_home, db) = open_state();
    let shared = "fn shared() -> u8 { 1 }";
    let (_worktree, generation) = seed_generation(&db, 1, &[shared, shared, "fn other() {}"]).await;
    let representation_id = require_kind(&db, REPRESENTATION_ID, RepresentationKind::CodeRaw).await;

    let read = db.open_read().expect("read");
    let generations: BTreeSet<String> = [generation].into_iter().collect();
    let set = expected_subject_keys(&read, DEFAULT_MODEL_SPACE_ID, &generations).expect("subjects");

    assert_eq!(set.keys.len(), 2, "three occurrences, two distinct blobs");
    assert!(set.unsupported.is_empty());
    let shared_key = EmbeddingKey {
        subject_kind: SubjectKind::ContentBlob,
        subject_hash: subject_content_blob(&derive_content_blob(LANGUAGE, shared).blob_id),
        representation_id: representation_id.clone(),
    };
    assert!(
        set.keys.contains(&shared_key),
        "the shared blob's subject must be present exactly once"
    );

    // Expected-per-kind mirrors the set (the coverage `expected` half).
    let per_kind = set.expected_per_kind(&[(RepresentationKind::CodeRaw, representation_id)]);
    assert_eq!(per_kind.get(&RepresentationKind::CodeRaw), Some(&2));
}

/// A required kind whose subject function does not exist yet is reported, not
/// silently dropped.
#[tokio::test(flavor = "multi_thread")]
async fn kinds_without_a_subject_function_are_reported() {
    let (_home, db) = open_state();
    let (_worktree, generation) = seed_generation(&db, 10, &["fn a() {}"]).await;
    require_kind(&db, REPRESENTATION_ID, RepresentationKind::CodeRaw).await;
    require_kind(
        &db,
        "88888888-8888-7888-8888-888888888888",
        RepresentationKind::CodeContext,
    )
    .await;

    let read = db.open_read().expect("read");
    let generations: BTreeSet<String> = [generation].into_iter().collect();
    let set = expected_subject_keys(&read, DEFAULT_MODEL_SPACE_ID, &generations).expect("subjects");

    assert_eq!(
        set.unsupported.iter().copied().collect::<Vec<_>>(),
        vec![RepresentationKind::CodeContext],
        "code_context's serialization is still [OPEN] (spec 09 §3)"
    );
    assert!(
        set.keys
            .iter()
            .all(|k| k.representation_id == REPRESENTATION_ID),
        "no key is minted for an unsupported kind"
    );
}

/// Pin roots span every worktree, so the expected set is store-wide.
#[tokio::test(flavor = "multi_thread")]
async fn pin_roots_cover_every_worktree() {
    let (_home, db) = open_state();
    let (_w1, g1) = seed_generation(&db, 20, &["fn a() {}"]).await;
    let (_w2, g2) = seed_generation(&db, 60, &["fn b() {}"]).await;
    require_kind(&db, REPRESENTATION_ID, RepresentationKind::CodeRaw).await;

    let read = db.open_read().expect("read");
    let roots = pinned_generations(&read, &params(), &ExternalPins::default(), NOW).expect("roots");
    assert!(roots.contains(&g1) && roots.contains(&g2), "{roots:?}");

    let set = expected_subject_keys(&read, DEFAULT_MODEL_SPACE_ID, &roots).expect("subjects");
    assert_eq!(set.keys.len(), 2, "one subject per worktree's single blob");
}

/// A model space that is still `building` is protected even though no worktree
/// references it — the pin that keeps a backfill from racing the LRU.
#[tokio::test(flavor = "multi_thread")]
async fn building_and_projection_ready_spaces_are_protected() {
    let (_home, db) = open_state();
    seed_generation(&db, 30, &["fn a() {}"]).await;

    let building = uuid(200);
    let (b, name) = (building.clone(), "space-b".to_string());
    db.writer()
        .transaction(move |tx| create_model_space(tx, &b, &name, NOW))
        .await
        .expect("create building space");

    let read = db.open_read().expect("read");
    let protected = protected_model_space_ids(&read).expect("protected");
    assert!(
        protected.contains(&building),
        "a `building` space must be protected: {protected:?}"
    );
    assert!(
        protected.contains(DEFAULT_MODEL_SPACE_ID),
        "the seeded default (referenced/active) space stays protected"
    );

    // The state reader that backs it agrees.
    let ids = model_space_ids_in_states(&read, &[ModelSpaceState::Building]).expect("ids");
    assert_eq!(ids, vec![building.clone()]);
    assert!(
        model_space_ids_in_states(&read, &[ModelSpaceState::Retiring])
            .expect("ids")
            .is_empty()
    );
    assert!(
        model_space_ids_in_states(&read, &[])
            .expect("ids")
            .is_empty(),
        "an empty state filter selects nothing"
    );
}

/// A `retiring` space that no worktree references is *not* protected — the
/// widening must not pin everything forever (spec 04 §3: its rows "become
/// evictable when no worktree references A").
#[tokio::test(flavor = "multi_thread")]
async fn a_retiring_unreferenced_space_is_not_protected() {
    let (_home, db) = open_state();
    seed_generation(&db, 40, &["fn a() {}"]).await;

    let retiring = uuid(210);
    let (r, name) = (retiring.clone(), "space-r".to_string());
    db.writer()
        .transaction(move |tx| create_model_space(tx, &r, &name, NOW))
        .await
        .expect("create space");
    for to in [
        ModelSpaceState::ProjectionReady,
        ModelSpaceState::Active,
        ModelSpaceState::Retiring,
    ] {
        let r = retiring.clone();
        db.writer()
            .transaction(move |tx| transition_model_space(tx, &r, to, &[], NOW))
            .await
            .expect("transition")
            .expect("legal");
    }

    let read = db.open_read().expect("read");
    let protected = protected_model_space_ids(&read).expect("protected");
    assert!(
        !protected.contains(&retiring),
        "an unreferenced retiring space must not be pinned: {protected:?}"
    );
}

/// The pin set eviction consumes is the union over protected spaces.
#[tokio::test(flavor = "multi_thread")]
async fn protected_keys_union_pin_roots_and_spaces() {
    let (_home, db) = open_state();
    let (_w, _g) = seed_generation(&db, 50, &["fn a() {}", "fn b() {}"]).await;
    require_kind(&db, REPRESENTATION_ID, RepresentationKind::CodeRaw).await;

    let read = db.open_read().expect("read");
    let keys =
        protected_subject_keys(&read, &params(), &ExternalPins::default(), NOW).expect("pins");
    assert_eq!(keys.len(), 2, "both blobs of the active generation");
    assert!(
        keys.iter()
            .all(|k| k.subject_kind == SubjectKind::ContentBlob)
    );
}
