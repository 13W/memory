//! T16-02 acceptance tests for `inspect <observation|memory|generation> <id>`
//! (spec 11 §6, 12 §3): full-row reads across all three kinds, evidence/audit
//! composition, payload TTL status, and a clean `None` for unknown ids.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::memory::{
    Actor, CreateMemoryOp, EvidenceInput, MemoryKind, MemoryOpOutcome, NormalizationStatus,
    NormalizationWrite, ReinforceMemoryOp, ScopeKind, UpsertOutcome, apply_create, apply_reinforce,
    upsert_normalization,
};
use local_rag_store::observation::{
    EvidenceKind, NewObservationEnvelope, TrustLevel, insert_envelope,
};
use local_rag_store::privacy::{inspect_generation, inspect_memory, inspect_observation};
use local_rag_store::registry::{
    WorktreeKind, allocate_generation, create_repository, create_worktree,
};
use local_rag_store::rusqlite::params;
use local_rag_store::{PayloadStatus, StateDb};
use local_rag_test_support::TempHome;

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// The text [`create_with_evidence`] writes; a normalization fixture must hash
/// exactly this, since `upsert_normalization` refuses a stale write.
const CREATED_TEXT: &str = "some durable text";

fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Seed one `observation_envelope`, optionally with paths and a payload.
async fn seed_observation(
    db: &StateDb,
    seed: u8,
    session_id: &'static str,
    paths: &'static [&'static str],
    payload: Option<(&'static [u8], i64)>,
) -> String {
    let observation_id = uuid(seed);
    let oid = observation_id.clone();
    db.writer()
        .transaction(move |tx| {
            insert_envelope(
                tx,
                &NewObservationEnvelope {
                    observation_id: &oid,
                    source_event_id: "evt-1",
                    dedup_key: None,
                    payload_hash: "deadbeef",
                    event_type: "Stop",
                    evidence_kind: "user_statement",
                    trust: "normal",
                    source_timestamp: Some(1000),
                    repo_id: None,
                    worktree_id: None,
                    session_id,
                    agent_id: None,
                    turn_id: None,
                    batch_id: None,
                    commit_hash: None,
                    short_evidence_excerpt: Some("short excerpt"),
                    redaction_version: Some(1),
                },
            )?;
            for path in paths {
                tx.execute(
                    "INSERT INTO observation_path (observation_id, normalized_path) VALUES (?1, ?2)",
                    params![oid, path],
                )?;
            }
            if let Some((bytes, expires_at)) = payload {
                tx.execute(
                    "INSERT INTO observation_payload \
                       (observation_id, redacted_payload, byte_size, expires_at) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![oid, bytes, bytes.len() as i64, expires_at],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed observation");
    observation_id
}

/// `apply_create` with the given evidence, returning the created memory_id.
async fn create_with_evidence(
    db: &StateDb,
    memory_id: &str,
    scope_owner_id: &str,
    evidence: Vec<(String, &'static str)>,
) -> MemoryOpOutcome {
    let (id, owner) = (memory_id.to_string(), scope_owner_id.to_string());
    db.writer()
        .transaction(move |tx| {
            let evidence_inputs: Vec<EvidenceInput<'_>> = evidence
                .iter()
                .map(|(oid, session)| EvidenceInput {
                    observation_id: oid,
                    evidence_kind: EvidenceKind::UserStatement,
                    session_id: session,
                    agent_id: None,
                    commit_hash: None,
                })
                .collect();
            apply_create(
                tx,
                &CreateMemoryOp {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: CREATED_TEXT,
                    canonical_key: None,
                    scope_kind: ScopeKind::Worktree,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    evidence: &evidence_inputs,
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("create applies")
}

#[tokio::test]
async fn inspect_observation_returns_every_field_and_paths() {
    let (_home, db) = open_state();
    let observation_id = seed_observation(
        &db,
        1,
        "sess-1",
        &["src/a.rs", "src/b.rs"],
        Some((b"hello world", 5000)),
    )
    .await;

    let read = db.open_read().expect("read conn");
    let found = inspect_observation(&read, &observation_id, 1000)
        .expect("query")
        .expect("row present");
    assert_eq!(found.observation_id, observation_id);
    assert_eq!(found.source_event_id, "evt-1");
    assert_eq!(found.event_type, "Stop");
    assert_eq!(found.evidence_kind, EvidenceKind::UserStatement);
    assert_eq!(found.trust, TrustLevel::Normal);
    assert_eq!(found.session_id, "sess-1");
    assert_eq!(
        found.short_evidence_excerpt.as_deref(),
        Some("short excerpt")
    );
    assert_eq!(found.redaction_version, Some(1));
    assert_eq!(
        found.paths,
        vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
    );
    assert_eq!(
        found.payload,
        PayloadStatus::Present {
            byte_size: 11,
            expires_at: 5000,
            text: "hello world".to_string(),
        }
    );
}

#[tokio::test]
async fn inspect_observation_of_unknown_id_is_none() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    assert_eq!(inspect_observation(&read, "unknown", 1000).unwrap(), None);
}

#[tokio::test]
async fn inspect_observation_payload_present_vs_expired_vs_none() {
    let (_home, db) = open_state();
    let live = seed_observation(&db, 10, "sess-1", &[], Some((b"live", 5000))).await;
    let expiring = seed_observation(&db, 11, "sess-1", &[], Some((b"gone", 5000))).await;
    let none = seed_observation(&db, 12, "sess-1", &[], None).await;

    let read = db.open_read().expect("read conn");
    assert!(matches!(
        inspect_observation(&read, &live, 4999)
            .unwrap()
            .unwrap()
            .payload,
        PayloadStatus::Present { .. }
    ));
    assert_eq!(
        inspect_observation(&read, &expiring, 5000)
            .unwrap()
            .unwrap()
            .payload,
        PayloadStatus::Expired { expires_at: 5000 },
    );
    assert_eq!(
        inspect_observation(&read, &none, 0)
            .unwrap()
            .unwrap()
            .payload,
        PayloadStatus::None,
    );
}

#[tokio::test]
async fn inspect_memory_includes_entry_evidence_and_audit_trail() {
    let (_home, db) = open_state();
    let owner = uuid(20);
    let obs_a = seed_observation(&db, 21, "sess-1", &[], Some((b"payload-a", 5000))).await;
    let obs_b = seed_observation(&db, 22, "sess-1", &[], None).await;
    let memory_id = uuid(23);

    let MemoryOpOutcome::Applied(created) = create_with_evidence(
        &db,
        &memory_id,
        &owner,
        vec![(obs_a.clone(), "sess-1"), (obs_b.clone(), "sess-1")],
    )
    .await
    else {
        panic!("expected Applied");
    };
    assert_eq!(created.entry_version, 1);

    let (mid, expected_version) = (memory_id.clone(), created.entry_version);
    db.writer()
        .transaction(move |tx| {
            apply_reinforce(
                tx,
                &ReinforceMemoryOp {
                    memory_id: &mid,
                    expected_version,
                    confidence: Some(0.9),
                    evidence: &[],
                    actor: Actor::User,
                    idempotency_key: None,
                },
                1000,
            )
        })
        .await
        .expect("infra")
        .expect("reinforce applies");

    let read = db.open_read().expect("read conn");
    let found = inspect_memory(&read, &memory_id, 1000)
        .expect("query")
        .expect("row present");
    assert_eq!(found.entry.memory_id, memory_id);
    assert_eq!(found.entry.entry_version, 2, "reinforce bumped the version");
    assert_eq!(found.entry.confidence, 0.9);

    let mut evidence_ids: Vec<&str> = found
        .evidence
        .iter()
        .map(|e| e.observation_id.as_str())
        .collect();
    evidence_ids.sort();
    assert_eq!(evidence_ids, vec![obs_a.as_str(), obs_b.as_str()]);
    let a = found
        .evidence
        .iter()
        .find(|e| e.observation_id == obs_a)
        .expect("obs_a evidence summary");
    assert!(matches!(a.payload, PayloadStatus::Present { .. }));
    let b = found
        .evidence
        .iter()
        .find(|e| e.observation_id == obs_b)
        .expect("obs_b evidence summary");
    assert_eq!(b.payload, PayloadStatus::None);

    assert_eq!(found.audit_trail.len(), 2, "create + reinforce");
    assert_eq!(found.audit_trail[0].op, "create");
    assert_eq!(found.audit_trail[0].entity_version, 1);
    assert_eq!(found.audit_trail[1].op, "reinforce");
    assert_eq!(found.audit_trail[1].entity_version, 2);
}

#[tokio::test]
async fn inspect_memory_of_unknown_id_is_none() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");
    assert_eq!(inspect_memory(&read, "unknown", 1000).unwrap(), None);
}

#[tokio::test]
async fn inspect_generation_reads_full_row_then_none_for_unknown_id() {
    let (_home, db) = open_state();
    let repo = uuid(30);
    let wt = uuid(31);
    let genr = uuid(32);
    let (repo0, wt0, genr0) = (repo.clone(), wt.clone(), genr.clone());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo0, None, 1000)?;
            create_worktree(tx, &wt0, &repo0, WorktreeKind::Main, 1000)?;
            allocate_generation(tx, &wt0, &genr0, 1000).map(|_| ())
        })
        .await
        .expect("seed generation");

    let read = db.open_read().expect("read conn");
    let found = inspect_generation(&read, &genr)
        .expect("query")
        .expect("row present");
    assert_eq!(found.generation_id, genr);
    assert_eq!(found.worktree_id, wt);
    assert_eq!(found.generation_number, 1);

    assert_eq!(inspect_generation(&read, "unknown").unwrap(), None);
}

// ---------------------------------------------------------------------------
// T21-07: the entry's English variant is part of what `inspect` shows
// ---------------------------------------------------------------------------

/// Write one normalization row for `memory_id` over the text
/// [`create_with_evidence`] wrote — `upsert_normalization` refuses anything
/// whose source hash does not match the entry as it stands.
async fn seed_normalization(
    db: &StateDb,
    memory_id: &str,
    status: NormalizationStatus,
    source_text: Option<&'static str>,
    last_error: Option<&'static str>,
) {
    let id = memory_id.to_string();
    let sha = local_rag_core::hash::sha256_hex(CREATED_TEXT.as_bytes());
    let outcome = db
        .writer()
        .transaction(move |tx| {
            upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status,
                    expected_text_sha256: &sha,
                    canon_text_sha256: &sha,
                    source_text,
                    source_language: Some("ru"),
                    normalizer_model_id: Some("test-normalizer"),
                    prompt_version: Some(1),
                    normalizer_version: 1,
                    attempt_count: if last_error.is_some() { 3 } else { 1 },
                    last_error,
                    next_attempt_at: None,
                },
                2000,
            )
        })
        .await
        .expect("infra");
    assert_eq!(
        outcome,
        UpsertOutcome::Written,
        "fixture must actually land"
    );
}

#[tokio::test]
async fn inspect_memory_carries_the_translation_and_its_provenance() {
    let (_home, db) = open_state();
    let owner = uuid(60);
    let id = uuid(61);
    create_with_evidence(&db, &id, &owner, vec![]).await;
    seed_normalization(
        &db,
        &id,
        NormalizationStatus::Translated,
        Some("the English variant"),
        None,
    )
    .await;

    let read = db.open_read().expect("read conn");
    let found = inspect_memory(&read, &id, 3000)
        .expect("infra")
        .expect("entry exists");
    let normalization = found.normalization.expect("the row is part of inspect");

    assert_eq!(normalization.status, NormalizationStatus::Translated);
    assert_eq!(
        normalization.source_text.as_deref(),
        Some("the English variant"),
        "an export of everything the store holds must include the stored translation",
    );
    assert_eq!(
        normalization.canon_text_sha256,
        local_rag_core::hash::sha256_hex(CREATED_TEXT.as_bytes()),
        "the hash says which text this translation belongs to",
    );
    assert_eq!(normalization.source_language.as_deref(), Some("ru"));
    assert_eq!(
        normalization.normalizer_model_id.as_deref(),
        Some("test-normalizer"),
    );
    assert_eq!(normalization.prompt_version, Some(1));
    assert_eq!(normalization.normalizer_version, 1);
    assert_eq!(
        found.entry.text, CREATED_TEXT,
        "the original is still the canonical text, untouched",
    );
}

#[tokio::test]
async fn inspect_memory_reports_a_failed_normalization_with_its_reason() {
    let (_home, db) = open_state();
    let owner = uuid(62);
    let id = uuid(63);
    create_with_evidence(&db, &id, &owner, vec![]).await;
    seed_normalization(
        &db,
        &id,
        NormalizationStatus::Failed,
        None,
        Some("answer was not one {\"en\": …} object"),
    )
    .await;

    let read = db.open_read().expect("read conn");
    let normalization = inspect_memory(&read, &id, 3000)
        .expect("infra")
        .expect("entry exists")
        .normalization
        .expect("a failed row is still a row");

    assert_eq!(normalization.status, NormalizationStatus::Failed);
    assert_eq!(normalization.source_text, None);
    assert_eq!(
        normalization.last_error.as_deref(),
        Some("answer was not one {\"en\": …} object"),
        "why there is no translation is provenance too",
    );
    assert_eq!(normalization.attempt_count, 3);
}

#[tokio::test]
async fn an_entry_that_was_never_normalized_reports_none() {
    let (_home, db) = open_state();
    let owner = uuid(64);
    let id = uuid(65);
    create_with_evidence(&db, &id, &owner, vec![]).await;

    let read = db.open_read().expect("read conn");
    let found = inspect_memory(&read, &id, 3000)
        .expect("infra")
        .expect("entry exists");
    assert!(
        found.normalization.is_none(),
        "no row is the honest answer on a store whose worker never ran",
    );
}
