//! T21-02 acceptance: the three readers of a memory entry's subject hash agree,
//! byte for byte, whether or not the entry has an English variant.
//!
//! The expected-key set (`memory_entry_subject_keys`, what backfill embeds
//! against and what eviction pins) and recall's dense leg
//! (`recall_candidates_for_scope` → `memory_entry_subject_hash`) derive the
//! same hash from opposite ends of the store. A disagreement between them is
//! invisible at runtime — the lookup simply finds no vector and the dense leg
//! returns nothing — so it is checked here instead.
//!
//! Deterministic: an isolated [`TempHome`], fixed `now_ms` literals, no clock,
//! no network.

#![cfg(unix)]

use std::collections::BTreeSet;

use local_rag_core::hash::sha256_hex;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    CURRENT_NORMALIZER_VERSION, GLOBAL_SCOPE_OWNER_ID, MemoryKind, MemoryState, NewMemoryEntry,
    NormalizationStatus, NormalizationWrite, ScopeKind, StateDb, UpsertOutcome,
    create_memory_entry, decide_effective_text, memory_entry_subject_hash,
    memory_entry_subject_keys, recall_candidates_for_scope, transition_memory_entry,
    upsert_normalization,
};
use local_rag_test_support::TempHome;

const REPRESENTATION: &str = "repr-memory-1";

fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

async fn seed_entry(db: &StateDb, memory_id: &str, text: &str) {
    let (id, text) = (memory_id.to_string(), text.to_string());
    db.writer()
        .transaction(move |tx| {
            create_memory_entry(
                tx,
                &NewMemoryEntry {
                    memory_id: &id,
                    kind: MemoryKind::Fact,
                    text: &text,
                    canonical_key: None,
                    scope_kind: ScopeKind::Global,
                    scope_owner_id: GLOBAL_SCOPE_OWNER_ID,
                    confidence: 0.5,
                    importance: 0.5,
                    valid_from_tree: None,
                    last_verified_tree: None,
                    supersedes_id: None,
                },
                1_000,
            )
        })
        .await
        .expect("create tx")
        .expect("create ok");
}

async fn normalize(db: &StateDb, memory_id: &str, source_text: &str, variant: &str) {
    let (id, sha, variant) = (
        memory_id.to_string(),
        sha256_hex(source_text.as_bytes()),
        variant.to_string(),
    );
    let outcome = db
        .writer()
        .transaction(move |tx| {
            upsert_normalization(
                tx,
                &NormalizationWrite {
                    memory_id: &id,
                    status: NormalizationStatus::Ready,
                    source_text_sha256: &sha,
                    normalized_text: Some(&variant),
                    source_language: Some("ru"),
                    normalizer_model_id: Some("test-model"),
                    prompt_version: Some(1),
                    normalizer_version: CURRENT_NORMALIZER_VERSION,
                    attempt_count: 1,
                    last_error: None,
                    next_attempt_at: None,
                },
                2_000,
            )
        })
        .await
        .expect("upsert tx");
    assert_eq!(outcome, UpsertOutcome::Written);
}

fn expected_hashes(db: &StateDb) -> BTreeSet<String> {
    let read = db.open_read().expect("read conn");
    memory_entry_subject_keys(&read, REPRESENTATION)
        .expect("expected keys")
        .into_iter()
        .map(|k| k.subject_hash)
        .collect()
}

fn recall_hashes(db: &StateDb) -> Vec<(String, String)> {
    let read = db.open_read().expect("read conn");
    recall_candidates_for_scope(&read, ScopeKind::Global, GLOBAL_SCOPE_OWNER_ID)
        .expect("candidates")
        .into_iter()
        .map(|c| {
            (
                c.memory_id.clone(),
                memory_entry_subject_hash(&c.embed_text),
            )
        })
        .collect()
}

/// With no normalization anywhere — the state of every store that upgrades to
/// v14 — both readers must produce exactly what they produced before this
/// group existed: the hash of the entry's own text.
#[tokio::test]
async fn without_any_normalization_both_readers_hash_the_original_text() {
    let (_home, db) = open_state();
    seed_entry(&db, "m-1", "первый текст").await;
    seed_entry(&db, "m-2", "second text").await;

    let expected = expected_hashes(&db);
    let recall = recall_hashes(&db);

    assert_eq!(expected.len(), 2, "one key per entry, no fan-out");
    for (memory_id, hash) in &recall {
        assert!(
            expected.contains(hash),
            "recall's hash for {memory_id} is not in the expected set",
        );
    }
    assert_eq!(
        recall[0].1,
        memory_entry_subject_hash(&decide_effective_text("m-1", "первый текст", None)),
        "the original text is what gets hashed",
    );
}

/// With a usable variant, both readers must move to the variant's hash
/// **together**. A reader left behind would look up a vector nobody wrote.
#[tokio::test]
async fn a_normalized_entry_moves_both_readers_to_the_same_new_hash() {
    let (_home, db) = open_state();
    seed_entry(&db, "m-1", "первый текст").await;
    seed_entry(&db, "m-2", "second text").await;
    let before = expected_hashes(&db);

    normalize(&db, "m-1", "первый текст", "first text").await;

    let expected = expected_hashes(&db);
    let recall = recall_hashes(&db);
    assert_eq!(expected.len(), 2, "still one key per entry — no fan-out");
    assert_ne!(expected, before, "normalizing an entry moves its subject");

    let variant_hash = memory_entry_subject_hash(&decide_effective_text("m-1", "first text", None));
    assert!(
        expected.contains(&variant_hash),
        "the expected set must now hold the variant's hash",
    );
    assert_eq!(
        recall.iter().find(|(id, _)| id == "m-1").unwrap().1,
        variant_hash,
        "recall must look up exactly that hash, byte for byte",
    );
    assert_eq!(
        recall.iter().find(|(id, _)| id == "m-2").unwrap().1,
        memory_entry_subject_hash(&decide_effective_text("m-2", "second text", None)),
        "an entry with no variant is untouched",
    );
}

/// A variant whose source text has since been edited is stale, and both
/// readers must fall back to the original together — back to the hash a vector
/// already exists for.
#[tokio::test]
async fn a_stale_variant_returns_both_readers_to_the_original() {
    let (_home, db) = open_state();
    seed_entry(&db, "m-1", "первый текст").await;
    normalize(&db, "m-1", "первый текст", "first text").await;

    db.writer()
        .transaction(|tx| {
            tx.execute(
                "UPDATE memory_entry SET text = 'переписанный текст' WHERE memory_id = 'm-1'",
                [],
            )
        })
        .await
        .expect("edit the text under the variant");

    let original_hash =
        memory_entry_subject_hash(&decide_effective_text("m-1", "переписанный текст", None));
    assert_eq!(
        expected_hashes(&db),
        BTreeSet::from([original_hash.clone()]),
    );
    assert_eq!(recall_hashes(&db), vec![("m-1".to_string(), original_hash)]);
}

/// Terminal entries are not recalled but still hold vectors worth pinning, so
/// the expected set is a superset of what recall asks for — and recall's own
/// order stays `ORDER BY memory_id`.
#[tokio::test]
async fn the_expected_set_covers_recall_and_the_candidate_order_is_stable() {
    let (_home, db) = open_state();
    seed_entry(&db, "m-c", "третий").await;
    seed_entry(&db, "m-a", "первый").await;
    seed_entry(&db, "m-b", "второй").await;
    db.writer()
        .transaction(|tx| transition_memory_entry(tx, "m-b", MemoryState::Retracted))
        .await
        .expect("transition tx")
        .expect("retract ok");

    let expected = expected_hashes(&db);
    let recall = recall_hashes(&db);

    assert_eq!(expected.len(), 3, "every entry, terminal ones included");
    assert_eq!(
        recall.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec!["m-a", "m-c"],
        "recall skips the terminal entry and stays ordered by memory_id",
    );
    for (memory_id, hash) in &recall {
        assert!(
            expected.contains(hash),
            "recall's hash for {memory_id} must be pinned by the expected set",
        );
    }
}
