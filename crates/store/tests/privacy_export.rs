//! T16-02 acceptance tests for `export [--scope …]` (spec 11 §6, 12 §3):
//! scope isolation, deterministic ordering/output, and payload-expired
//! reporting.
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and ids minted from [`uuidv7_from`] with fixed entropy.

use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::memory::{
    NewMemoryEntry, NormalizationStatus, NormalizationWrite, ScopeKind, UpsertOutcome,
    create_memory_entry, upsert_normalization,
};
use local_rag_store::observation::{EvidenceKind, NewObservationEnvelope, insert_envelope};
use local_rag_store::privacy::export_scope;
use local_rag_store::rusqlite::params;
use local_rag_store::{NewMemoryEvidence, PayloadStatus, StateDb, insert_memory_evidence};
use local_rag_test_support::TempHome;

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// The text [`create_memory`] writes; a normalization fixture must hash exactly
/// this, since `upsert_normalization` refuses a stale write.
const CREATED_TEXT: &str = "some durable text";

fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

async fn seed_observation(db: &StateDb, seed: u8, payload: Option<(&'static [u8], i64)>) -> String {
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
                    session_id: "sess-1",
                    agent_id: None,
                    turn_id: None,
                    batch_id: None,
                    commit_hash: None,
                    short_evidence_excerpt: None,
                    redaction_version: Some(1),
                },
            )?;
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

/// Create one `memory_entry` (no evidence) at `created_at`, in `scope`.
async fn create_memory(
    db: &StateDb,
    memory_id: &str,
    scope_kind: ScopeKind,
    scope_owner_id: &str,
    created_at: i64,
) {
    let (id, owner) = (memory_id.to_string(), scope_owner_id.to_string());
    db.writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: local_rag_store::MemoryKind::Fact,
                    text: CREATED_TEXT,
                    canonical_key: None,
                    scope_kind,
                    scope_owner_id: &owner,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                created_at,
            )
        })
        .await
        .expect("infra")
        .expect("create applies");
}

async fn link_evidence(db: &StateDb, memory_id: &str, observation_id: &str) {
    let (mid, oid) = (memory_id.to_string(), observation_id.to_string());
    db.writer()
        .transaction(move |tx| {
            insert_memory_evidence(
                tx,
                &NewMemoryEvidence {
                    memory_id: &mid,
                    observation_id: &oid,
                    evidence_kind: EvidenceKind::UserStatement,
                    session_id: "sess-1",
                    agent_id: None,
                    commit_hash: None,
                },
            )
        })
        .await
        .expect("insert evidence");
}

#[tokio::test]
async fn export_scope_isolates_by_scope() {
    let (_home, db) = open_state();
    let repo_a = uuid(1);
    let repo_b = uuid(2);
    create_memory(&db, &uuid(3), ScopeKind::Repository, &repo_a, 1000).await;
    create_memory(&db, &uuid(4), ScopeKind::Repository, &repo_b, 1000).await;

    let read = db.open_read().expect("read conn");
    let exported =
        export_scope(&read, &[(ScopeKind::Repository, repo_a.clone())], 1000).expect("export");
    assert_eq!(exported.len(), 1, "only repo_a's entry is exported");
    assert_eq!(exported[0].entry.scope_owner_id, repo_a);
}

#[tokio::test]
async fn export_scope_orders_deterministically_by_created_at_then_memory_id() {
    let (_home, db) = open_state();
    let owner = uuid(10);
    // Seed out of created_at order and with a tie to prove the memory_id tiebreak.
    create_memory(&db, &uuid(13), ScopeKind::Worktree, &owner, 3000).await;
    create_memory(&db, &uuid(11), ScopeKind::Worktree, &owner, 1000).await;
    create_memory(&db, &uuid(12), ScopeKind::Worktree, &owner, 1000).await;

    let read = db.open_read().expect("read conn");
    let exported = export_scope(&read, &[(ScopeKind::Worktree, owner)], 1000).expect("export");
    let ids: Vec<&str> = exported
        .iter()
        .map(|e| e.entry.memory_id.as_str())
        .collect();
    let expected = [uuid(11), uuid(12), uuid(13)];
    assert_eq!(
        ids,
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "created_at ascending, tie broken by memory_id ascending"
    );
}

#[tokio::test]
async fn export_scope_reports_payload_expired() {
    let (_home, db) = open_state();
    let owner = uuid(20);
    let live_obs = seed_observation(&db, 21, Some((b"live", 9000))).await;
    let expired_obs = seed_observation(&db, 22, Some((b"gone", 5000))).await;
    let memory_id = uuid(23);
    create_memory(&db, &memory_id, ScopeKind::Worktree, &owner, 1000).await;
    link_evidence(&db, &memory_id, &live_obs).await;
    link_evidence(&db, &memory_id, &expired_obs).await;

    let read = db.open_read().expect("read conn");
    let exported = export_scope(&read, &[(ScopeKind::Worktree, owner)], 5000).expect("export");
    assert_eq!(exported.len(), 1);
    let live = exported[0]
        .evidence
        .iter()
        .find(|e| e.observation_id == live_obs)
        .expect("live evidence");
    assert_eq!(
        live.payload,
        PayloadStatus::Present {
            byte_size: 4,
            expires_at: 9000,
            text: "live".to_string(),
        },
        "not yet expired at now_ms=5000"
    );
    let expired = exported[0]
        .evidence
        .iter()
        .find(|e| e.observation_id == expired_obs)
        .expect("expired evidence");
    assert_eq!(
        expired.payload,
        PayloadStatus::Expired { expires_at: 5000 },
        "expires_at <= now_ms is expired, the sweep's own <= convention"
    );
}

#[tokio::test]
async fn export_scope_is_byte_identical_across_two_calls_on_unchanged_state() {
    let (_home, db) = open_state();
    let owner = uuid(30);
    let obs = seed_observation(&db, 31, Some((b"stable", 5000))).await;
    let memory_id = uuid(32);
    create_memory(&db, &memory_id, ScopeKind::Worktree, &owner, 1000).await;
    link_evidence(&db, &memory_id, &obs).await;

    let read = db.open_read().expect("read conn");
    let first =
        export_scope(&read, &[(ScopeKind::Worktree, owner.clone())], 1000).expect("export 1");
    let second = export_scope(&read, &[(ScopeKind::Worktree, owner)], 1000).expect("export 2");
    assert_eq!(first, second, "identical state must export identically");
}

#[tokio::test]
async fn export_scope_includes_full_audit_trail_per_entry() {
    let (_home, db) = open_state();
    let owner = uuid(40);
    let memory_id = uuid(41);
    create_memory(&db, &memory_id, ScopeKind::Worktree, &owner, 1000).await;
    // create_memory_entry alone writes no audit_event -- confirm the export
    // still succeeds with an empty trail rather than erroring.
    let read = db.open_read().expect("read conn");
    let exported = export_scope(&read, &[(ScopeKind::Worktree, owner)], 1000).expect("export");
    assert_eq!(exported.len(), 1);
    assert_eq!(exported[0].audit_trail, Vec::new());
}

// ---------------------------------------------------------------------------
// T21-07: an export shows the English variant too
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_carries_the_translation_and_leaves_unnormalized_entries_as_none() {
    let (_home, db) = open_state();
    let owner = uuid(80);
    let normalized = uuid(81);
    let plain = uuid(82);
    create_memory(&db, &normalized, ScopeKind::Worktree, &owner, 1000).await;
    create_memory(&db, &plain, ScopeKind::Worktree, &owner, 2000).await;

    let id = normalized.clone();
    let sha = local_rag_core::hash::sha256_hex(CREATED_TEXT.as_bytes());
    let outcome = db
        .writer()
        .transaction(move |tx| {
            upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status: NormalizationStatus::Ready,
                    source_text_sha256: &sha,
                    normalized_text: Some("the English variant"),
                    source_language: Some("ru"),
                    normalizer_model_id: Some("test-normalizer"),
                    prompt_version: Some(1),
                    normalizer_version: 1,
                    attempt_count: 1,
                    last_error: None,
                    next_attempt_at: None,
                },
                3000,
            )
        })
        .await
        .expect("infra");
    assert_eq!(outcome, UpsertOutcome::Written);

    let read = db.open_read().expect("read conn");
    let exported = export_scope(&read, &[(ScopeKind::Worktree, owner.clone())], 4000)
        .expect("export succeeds");

    assert_eq!(exported.len(), 2);
    let first = &exported[0];
    assert_eq!(first.entry.memory_id, normalized);
    let row = first
        .normalization
        .as_ref()
        .expect("export is never poorer than inspect");
    assert_eq!(
        row.normalized_text.as_deref(),
        Some("the English variant"),
        "an export exists to show everything the store holds about the user",
    );
    assert_eq!(row.normalizer_model_id.as_deref(), Some("test-normalizer"));

    assert_eq!(exported[1].entry.memory_id, plain);
    assert!(exported[1].normalization.is_none());
}
