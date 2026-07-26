//! A seeded store the backfill worker can run against.
//!
//! Builds the same shape `crates/store/tests/embedding_cache.rs` builds by hand —
//! repository → worktree → generation → file revisions/units/occurrences →
//! registered `code_raw` representation → active projection tuple — but as a
//! reusable fixture, since every backfill test needs all of it.
//!
//! Deterministic throughout: fixed ids from `uuidv7_from` with pinned entropy, a
//! fixed `NOW`, an isolated `TempHome`, no network and no sleeps.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::code::{NewFileRevision, NewOccurrence, NewParsedUnit, UnitKind};
use local_rag_store::registry::{
    DEFAULT_MODEL_SPACE_ID, DistanceMetric, ProjectionStateChange, ProjectionStatus,
    RepresentationKey, RepresentationKind, WorktreeKind, allocate_generation, create_repository,
    create_worktree, insert_projection_state, register_representation,
    set_model_space_representation, write_projection_state,
};
use local_rag_store::{
    CacheDb, DerivedContentBlob, GenerationState, StateDb, create_or_reuse_content_blob,
    derive_content_blob, insert_file_revision, insert_generation_file, insert_occurrence,
    insert_parsed_unit, occurrence_id, transition_generation,
};
use local_rag_test_support::TempHome;

pub const STORE_UUID: &str = "44444444-4444-7444-8444-444444444444";
pub const NOW: i64 = 1_000_000;
pub const REPRESENTATION_ID: &str = "55555555-5555-7555-8555-555555555555";
pub const LANGUAGE: &str = "rust";

/// A distinct, deterministic UUIDv7 string; `seed` varies the last entropy byte.
pub fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// A seeded store: the two databases plus the ids a test needs to address them.
pub struct Fixture {
    pub _home: TempHome,
    pub state: StateDb,
    pub cache: CacheDb,
    pub worktree_id: String,
    pub generation_id: String,
    /// `(occurrence_id, blob_id)` in insertion order.
    pub occurrences: Vec<(String, String)>,
}

impl Fixture {
    /// Distinct content blobs across the generation — the expected subject count
    /// for `code_raw` (occurrences sharing a blob collapse to one subject).
    pub fn distinct_blobs(&self) -> usize {
        let mut ids: Vec<&str> = self.occurrences.iter().map(|(_, b)| b.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    }
}

/// Seed a store whose active generation holds one occurrence per entry of
/// `bodies`; entries with identical text share one content blob by construction
/// (content addressing), which is what the sharing test relies on.
pub async fn seeded(bodies: &[&str]) -> Fixture {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let cache = CacheDb::open(layout.cache_db(), STORE_UUID).expect("open cache.sqlite");

    let repo_id = uuid(1);
    let worktree_id = uuid(2);
    let (r, w) = (repo_id.clone(), worktree_id.clone());
    state
        .writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, NOW)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, NOW)
        })
        .await
        .expect("seed repo + worktree");

    let generation_id = uuid(3);
    let (w, g) = (worktree_id.clone(), generation_id.clone());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, NOW))
        .await
        .expect("allocate generation");

    let mut occurrences = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        let file_revision_id = uuid(10 + i as u8);
        let unit_id = uuid(60 + i as u8);
        let path = format!("src/file_{i}.rs");
        let blob_id = seed_unit(&state, &file_revision_id, &unit_id, body).await;
        let occ = seed_occurrence(&state, &generation_id, &path, &file_revision_id, &unit_id).await;
        occurrences.push((occ, blob_id));
    }

    // A generation must be `projection_ready`/`active` to be a pin root (spec 06
    // §5); the backfill's expected set is defined over pin roots.
    let g = generation_id.clone();
    state
        .writer()
        .transaction(move |tx| transition_generation(tx, &g, GenerationState::ProjectionReady))
        .await
        .expect("generation → projection_ready")
        .expect("legal transition");

    register_code_raw(&state).await;
    establish_active_tuple(&state, &worktree_id, &generation_id).await;

    Fixture {
        _home: home,
        state,
        cache,
        worktree_id,
        generation_id,
        occurrences,
    }
}

/// One file revision + content blob + parsed unit holding `body`.
async fn seed_unit(state: &StateDb, file_revision_id: &str, unit_id: &str, body: &str) -> String {
    let derived = derive_content_blob(LANGUAGE, body);
    let (fr, u, blob, bytes) = (
        file_revision_id.to_string(),
        unit_id.to_string(),
        derived.blob_id.clone(),
        body.as_bytes().to_vec(),
    );
    let len = bytes.len() as i64;
    let (algo, norm) = (derived.algo_version, derived.normalization_version);
    let normalized = derived.normalized_text.clone();
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
            // Two units may share one blob (identical text is content-addressed
            // to the same id) — reuse instead of inserting twice.
            create_or_reuse_content_blob(
                tx,
                &DerivedContentBlob {
                    blob_id: blob.clone(),
                    normalized_text: normalized.clone(),
                    byte_size: normalized.len() as i64,
                    algo_version: algo,
                    normalization_version: norm,
                },
                LANGUAGE,
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
                    local_name: None,
                    kind: None,
                    parent_unit_id: None,
                },
            )
        })
        .await
        .expect("seed unit");
    derived.blob_id
}

/// Bind `unit_id` to `normalized_path` as a member + occurrence of the generation.
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

/// Register the bootstrap `code_raw` representation and mark it `required` on the
/// default model space — the key the `HashingEmbedder` itself reports.
pub async fn register_code_raw(state: &StateDb) -> String {
    let key = local_rag_embed::Embedder::key(&local_rag_embed::HashingEmbedder::new(
        RepresentationKind::CodeRaw,
    ));
    register_kind(state, REPRESENTATION_ID, key).await
}

/// Register `key` and attach it as a `required` representation of the default
/// model space.
pub async fn register_kind(
    state: &StateDb,
    representation_id: &str,
    key: RepresentationKey,
) -> String {
    let (id, kind) = (representation_id.to_string(), key.kind);
    state
        .writer()
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
        .expect("register representation")
}

/// A representation key that is *not* the bootstrap one (different `model_id`),
/// for the registry-mismatch cases.
pub fn foreign_key(kind: RepresentationKind) -> RepresentationKey {
    RepresentationKey {
        kind,
        representation_version: 1,
        normalization_version: 1,
        model_id: "some-other-model".to_string(),
        dimensions: 8,
        distance_metric: DistanceMetric::Cosine,
    }
}

/// Establish the worktree's active projection tuple through the same
/// write-ahead-then-commit two-step the real switch protocol uses (spec 04 §2).
async fn establish_active_tuple(state: &StateDb, worktree_id: &str, generation_id: &str) {
    let w = worktree_id.to_string();
    state
        .writer()
        .transaction(move |tx| insert_projection_state(tx, &w, NOW))
        .await
        .expect("init projection state");

    let op = uuid(250);
    let (w1, g1, op1) = (
        worktree_id.to_string(),
        generation_id.to_string(),
        op.clone(),
    );
    state
        .writer()
        .transaction(move |tx| {
            write_projection_state(
                tx,
                &w1,
                &ProjectionStateChange {
                    status_to: Some(ProjectionStatus::Updating),
                    target_generation_id: Some(g1),
                    target_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
                    projection_op_id: Some(op1),
                    ..Default::default()
                },
                NOW,
            )
        })
        .await
        .expect("write-ahead")
        .expect("write-ahead legal");

    let (w2, g2, op2) = (worktree_id.to_string(), generation_id.to_string(), op);
    state
        .writer()
        .transaction(move |tx| {
            write_projection_state(
                tx,
                &w2,
                &ProjectionStateChange {
                    status_to: Some(ProjectionStatus::Clean),
                    active_generation_id: Some(g2.clone()),
                    active_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
                    projected_generation_id: Some(g2),
                    projected_model_space_id: Some(DEFAULT_MODEL_SPACE_ID.to_string()),
                    projection_op_id: Some(op2),
                    ..Default::default()
                },
                NOW,
            )
        })
        .await
        .expect("commit")
        .expect("commit legal");
}
