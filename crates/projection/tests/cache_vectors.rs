//! `CacheVectorSource` acceptance tests: the `occurrence → embedding_cache`
//! bridge (spec 05 §5 step 1, 03 §4.2; `code_context` added by D-016).
//!
//! Both kinds are exercised against one seeded generation so the difference
//! under test is the *bridge*, not the fixture: `code_raw` resolves through
//! `parsed_unit.blob_id → H(subject/content_blob …)`, `code_context` through the
//! rendered envelope's own hash. A row that fails its checksum must read as
//! missing for either — that is the coverage guard's whole basis.
//!
//! Deterministic: isolated `TempHome`, fixed `now_ms`, ids from `uuidv7_from`
//! with pinned entropy, no network, no sleeps.

use local_rag_core::identity::domain::subject_content_blob;
use local_rag_core::identity::{Uuid, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_projection::{CacheVectorSource, RepresentationKind, VectorSource};
use local_rag_store::{
    CacheDb, DEFAULT_MODEL_SPACE_ID, DerivedContentBlob, DistanceMetric, EmbeddingKey,
    GenerationState, NewFileRevision, NewOccurrence, NewParsedUnit, NewlineStyle,
    RepresentationKey, SourceCompression, StateDb, SubjectKind, UnitKind, WorktreeKind,
    allocate_generation, context_subjects_for_generation, create_or_reuse_content_blob,
    create_repository, create_worktree, derive_content_blob, insert_embedding,
    insert_file_revision, insert_generation_file, insert_occurrence, insert_parsed_unit,
    occurrence_id, register_representation, rusqlite, set_model_space_representation,
    transition_generation,
};
use local_rag_test_support::TempHome;

const NOW: i64 = 1_000;
const DIMS: usize = 3;
const LANGUAGE: &str = "rust";
const BODY: &str = "fn parse(input: &str) -> Ast { todo!() }";
const PATH: &str = "src/parse.rs";

fn uuid(seed: u8) -> Uuid {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand)
}

/// A store whose active generation holds exactly one occurrence, plus both
/// representations registered on the default model space.
struct Fixture {
    _home: TempHome,
    state: StateDb,
    cache: CacheDb,
    generation_id: Uuid,
    model_space_id: Uuid,
    occurrence: String,
    blob_id: String,
    raw_representation: String,
    context_representation: String,
}

async fn seed() -> Fixture {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let cache = CacheDb::open(layout.cache_db(), &uuid(9).to_string()).expect("open cache.sqlite");

    let (repo, worktree, generation) = (uuid(1), uuid(2), uuid(3));
    let (r, w) = (repo.to_string(), worktree.to_string());
    state
        .writer()
        .transaction(move |tx| {
            create_repository(tx, &r, None, NOW)?;
            create_worktree(tx, &w, &r, WorktreeKind::Main, NOW)
        })
        .await
        .expect("repo + worktree");

    let (w, g) = (worktree.to_string(), generation.to_string());
    state
        .writer()
        .transaction(move |tx| allocate_generation(tx, &w, &g, NOW))
        .await
        .expect("allocate generation");

    let (file_revision, unit) = (uuid(4).to_string(), uuid(5).to_string());
    let derived = derive_content_blob(LANGUAGE, BODY);
    let blob_id = derived.blob_id.clone();
    let occurrence = occurrence_id(&generation.to_string(), PATH, &unit);

    let (f, u, o, g) = (
        file_revision.clone(),
        unit.clone(),
        occurrence.clone(),
        generation.to_string(),
    );
    let bytes = BODY.as_bytes().to_vec();
    let len = bytes.len() as i64;
    state
        .writer()
        .transaction(move |tx| {
            insert_file_revision(
                tx,
                &NewFileRevision {
                    file_revision_id: &f,
                    content_hash: &f,
                    parser_fingerprint: "fp",
                    source_blob: &bytes,
                    compression: SourceCompression::None,
                    source_encoding: "utf-8",
                    newline_style: NewlineStyle::Lf,
                    source_size: len,
                },
                NOW,
            )?;
            create_or_reuse_content_blob(
                tx,
                &DerivedContentBlob {
                    blob_id: derived.blob_id.clone(),
                    normalized_text: derived.normalized_text.clone(),
                    byte_size: derived.byte_size,
                    algo_version: derived.algo_version,
                    normalization_version: derived.normalization_version,
                },
                LANGUAGE,
                NOW,
            )?;
            insert_parsed_unit(
                tx,
                &NewParsedUnit {
                    unit_id: &u,
                    file_revision_id: &f,
                    unit_kind: UnitKind::Symbol,
                    syntax_locator: &format!("loc:{u}"),
                    blob_id: &derived.blob_id,
                    span_start: 0,
                    span_end: len,
                    local_name: Some("parse"),
                    kind: Some("function"),
                    parent_unit_id: None,
                },
            )?;
            insert_generation_file(tx, &g, PATH, PATH, &f)?;
            insert_occurrence(
                tx,
                &NewOccurrence {
                    occurrence_id: &o,
                    generation_id: &g,
                    normalized_path: PATH,
                    unit_id: &u,
                    qualified_name: None,
                    context_hash: None,
                },
            )
        })
        .await
        .expect("seed unit + occurrence");

    let g = generation.to_string();
    state
        .writer()
        .transaction(move |tx| transition_generation(tx, &g, GenerationState::ProjectionReady))
        .await
        .expect("transition tx")
        .expect("legal transition");

    let model_space_id: Uuid = DEFAULT_MODEL_SPACE_ID.parse().expect("default space id");
    let mut ids = Vec::new();
    for (i, kind) in [
        local_rag_store::RepresentationKind::CodeRaw,
        local_rag_store::RepresentationKind::CodeContext,
    ]
    .into_iter()
    .enumerate()
    {
        let representation_id = uuid(20 + i as u8).to_string();
        let id = state
            .writer()
            .transaction(move |tx| {
                let id = register_representation(
                    tx,
                    &representation_id,
                    &RepresentationKey {
                        kind,
                        representation_version: 1,
                        normalization_version: 1,
                        model_id: "vector-source-test".to_string(),
                        dimensions: DIMS as u32,
                        distance_metric: DistanceMetric::Cosine,
                    },
                    NOW,
                )?;
                set_model_space_representation(tx, DEFAULT_MODEL_SPACE_ID, kind, &id, true, NOW)?;
                Ok(id)
            })
            .await
            .expect("register representation");
        ids.push(id);
    }

    Fixture {
        _home: home,
        state,
        cache,
        generation_id: generation,
        model_space_id,
        occurrence,
        blob_id,
        raw_representation: ids[0].clone(),
        context_representation: ids[1].clone(),
    }
}

impl Fixture {
    /// The `code_context` subject hash the reader derives for the occurrence.
    fn context_hash(&self) -> String {
        let read = self.state.open_read().expect("state read");
        context_subjects_for_generation(&read, &self.generation_id.to_string())
            .expect("context subjects")
            .into_iter()
            .find(|s| s.occurrence_id == self.occurrence)
            .expect("the seeded occurrence has a context subject")
            .subject_hash
    }

    async fn cache_vector(&self, key: EmbeddingKey, vector: Vec<f32>) {
        self.cache
            .writer()
            .transaction(move |tx| insert_embedding(tx, &key, DIMS as i64, &vector, NOW))
            .await
            .expect("insert embedding");
    }

    fn source(&self) -> CacheVectorSource<'_> {
        let read = self.state.open_read().expect("state read");
        CacheVectorSource::new(
            &self.state,
            &self.cache,
            &read,
            &self.generation_id,
            &self.model_space_id,
        )
        .expect("vector source")
    }
}

/// Each kind resolves through its own bridge and finds its own vector.
///
/// Distinct vectors, so a bridge that fell through to the other kind's row would
/// return the wrong one rather than merely returning something.
#[tokio::test(flavor = "multi_thread")]
async fn both_kinds_resolve_to_their_own_cached_vector() {
    let f = seed().await;
    f.cache_vector(
        EmbeddingKey {
            subject_kind: SubjectKind::ContentBlob,
            subject_hash: subject_content_blob(&f.blob_id),
            representation_id: f.raw_representation.clone(),
        },
        vec![1.0, 0.0, 0.0],
    )
    .await;
    f.cache_vector(
        EmbeddingKey {
            subject_kind: SubjectKind::OccurrenceContext,
            subject_hash: f.context_hash(),
            representation_id: f.context_representation.clone(),
        },
        vec![0.0, 1.0, 0.0],
    )
    .await;

    let source = f.source();
    assert_eq!(
        source.vector(&f.occurrence, RepresentationKind::CodeRaw),
        Some(vec![1.0, 0.0, 0.0])
    );
    assert_eq!(
        source.vector(&f.occurrence, RepresentationKind::CodeContext),
        Some(vec![0.0, 1.0, 0.0])
    );
}

/// An uncached subject is `None`, never a substitute: `code_context` present and
/// `code_raw` absent must not make the context vector answer for both.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_row_is_none_per_kind() {
    let f = seed().await;
    f.cache_vector(
        EmbeddingKey {
            subject_kind: SubjectKind::OccurrenceContext,
            subject_hash: f.context_hash(),
            representation_id: f.context_representation.clone(),
        },
        vec![0.0, 1.0, 0.0],
    )
    .await;

    let source = f.source();
    assert_eq!(
        source.vector(&f.occurrence, RepresentationKind::CodeRaw),
        None
    );
    assert!(
        source
            .vector(&f.occurrence, RepresentationKind::CodeContext)
            .is_some()
    );
}

/// A row whose checksum no longer matches its bytes reads as missing — for
/// `code_context` exactly as for `code_raw` (spec 05 §7's coverage guard).
#[tokio::test(flavor = "multi_thread")]
async fn a_corrupt_context_row_reads_as_missing() {
    let f = seed().await;
    let hash = f.context_hash();
    f.cache_vector(
        EmbeddingKey {
            subject_kind: SubjectKind::OccurrenceContext,
            subject_hash: hash.clone(),
            representation_id: f.context_representation.clone(),
        },
        vec![0.0, 1.0, 0.0],
    )
    .await;
    assert!(
        f.source()
            .vector(&f.occurrence, RepresentationKind::CodeContext)
            .is_some(),
        "precondition: the row is readable before corruption"
    );

    // Flip the stored bytes without touching the checksum.
    let (h, r) = (hash, f.context_representation.clone());
    f.cache
        .writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE embedding_cache SET vector_f32 = ?1 \
                 WHERE subject_kind = 'occurrence_context' AND subject_hash = ?2 \
                   AND representation_id = ?3",
                rusqlite::params![vec![0u8; DIMS * 4], h, r],
            )
            .map(|_| ())
        })
        .await
        .expect("corrupt the row");

    assert_eq!(
        f.source()
            .vector(&f.occurrence, RepresentationKind::CodeContext),
        None,
        "a checksum mismatch is missing, not usable"
    );
}

/// An occurrence outside the source's generation has no context subject, so it
/// resolves to `None` rather than to some other generation's envelope.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_occurrence_is_none() {
    let f = seed().await;
    f.cache_vector(
        EmbeddingKey {
            subject_kind: SubjectKind::OccurrenceContext,
            subject_hash: f.context_hash(),
            representation_id: f.context_representation.clone(),
        },
        vec![0.0, 1.0, 0.0],
    )
    .await;

    let source = f.source();
    assert_eq!(
        source.vector("no-such-occurrence", RepresentationKind::CodeContext),
        None
    );
    assert_eq!(
        source.vector("no-such-occurrence", RepresentationKind::CodeRaw),
        None
    );
}
