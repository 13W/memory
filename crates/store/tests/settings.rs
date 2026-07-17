//! T02-05 acceptance tests for per-repository settings and the effective
//! `data_policy` merge (spec 02 §3.2, 03 §2.1, 12 §1).
//!
//! All tests are deterministic: an isolated [`TempHome`], fixed `now_ms`
//! literals, and `repo_id`s minted from [`uuidv7_from`] with fixed entropy (no
//! `SystemUuidV7`, so no wall clock or `/dev/urandom`). Writer operations run
//! through [`StateWriter::transaction`]; reads use [`StateDb::open_read`].

use local_rag_core::DataPolicy;
use local_rag_core::identity::uuidv7_from;
use local_rag_core::paths::StoreLayout;
use local_rag_store::registry::{
    DATA_POLICY_KEY, create_repository, effective_data_policy, get_repo_setting, repo_data_policy,
    repo_settings, set_repo_data_policy, set_repo_setting,
};
use local_rag_store::rusqlite::{Connection, Error, params};
use local_rag_store::{StateDb, WriteError};
use local_rag_test_support::TempHome;

/// A temporary store with an ensured tree and an opened [`StateDb`] (which runs
/// the production migration set, including registry v1 → `repo_settings`).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// A distinct, deterministic UUIDv7 string; `seed` varies the last entropy byte.
fn repo_id(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

/// Create a repository (no path observed — settings do not need one).
async fn create_repo(db: &StateDb, repo_id: &str) {
    let id = repo_id.to_string();
    db.writer()
        .transaction(move |tx| create_repository(tx, &id, None, 1000))
        .await
        .expect("create repository");
}

/// Set one generic setting.
async fn set(db: &StateDb, repo_id: &str, key: &str, value: &str) {
    let (id, k, v) = (repo_id.to_string(), key.to_string(), value.to_string());
    db.writer()
        .transaction(move |tx| set_repo_setting(tx, &id, &k, &v))
        .await
        .expect("set repo setting");
}

/// Set a repository's typed data policy.
async fn set_policy(db: &StateDb, repo_id: &str, policy: DataPolicy) {
    let id = repo_id.to_string();
    db.writer()
        .transaction(move |tx| set_repo_data_policy(tx, &id, policy))
        .await
        .expect("set repo data_policy");
}

/// Count a repository's `repo_settings` rows for `key`.
fn setting_row_count(conn: &Connection, repo_id: &str, key: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM repo_settings WHERE repo_id = ?1 AND key = ?2",
        params![repo_id, key],
        |r| r.get(0),
    )
    .expect("count settings rows")
}

const ALL_POLICIES: [DataPolicy; 4] = [
    DataPolicy::LocalOnly,
    DataPolicy::MetadataOnlyRemote,
    DataPolicy::AllowRemoteWithRedaction,
    DataPolicy::AllowRemoteFull,
];

#[tokio::test]
async fn generic_setting_round_trips() {
    let (_home, db) = open_state();
    let id = repo_id(1);
    create_repo(&db, &id).await;
    set(&db, &id, "default_model_space", "fast").await;

    let read = db.open_read().expect("read conn");
    assert_eq!(
        get_repo_setting(&read, &id, "default_model_space").expect("get"),
        Some("fast".to_string()),
    );
    assert_eq!(
        get_repo_setting(&read, &id, "missing").expect("get missing"),
        None,
    );
}

#[tokio::test]
async fn set_setting_is_idempotent_upsert() {
    let (_home, db) = open_state();
    let id = repo_id(2);
    create_repo(&db, &id).await;

    set(&db, &id, "k", "v1").await;
    set(&db, &id, "k", "v2").await; // overwrite, same key

    let read = db.open_read().expect("read conn");
    assert_eq!(
        setting_row_count(&read, &id, "k"),
        1,
        "a repeated key must not create a duplicate row",
    );
    assert_eq!(
        get_repo_setting(&read, &id, "k").expect("get"),
        Some("v2".to_string()),
        "the latest write wins",
    );
}

#[tokio::test]
async fn set_setting_on_unknown_repo_is_rejected() {
    let (_home, db) = open_state();
    let ghost = repo_id(3); // never created

    let g = ghost.clone();
    let result = db
        .writer()
        .transaction(move |tx| set_repo_setting(tx, &g, "k", "v"))
        .await;
    assert!(
        matches!(result, Err(WriteError::Sqlite(_))),
        "an unknown repo_id must be rejected by the foreign key, got {result:?}",
    );

    let read = db.open_read().expect("read conn");
    assert_eq!(
        setting_row_count(&read, &ghost, "k"),
        0,
        "nothing is written on rejection",
    );
}

#[tokio::test]
async fn data_policy_round_trips_typed() {
    let (_home, db) = open_state();
    let id = repo_id(4);
    create_repo(&db, &id).await;

    for policy in ALL_POLICIES {
        set_policy(&db, &id, policy).await;
        let read = db.open_read().expect("read conn");
        assert_eq!(
            repo_data_policy(&read, &id).expect("read policy"),
            Some(policy)
        );
        // Stored under the canonical mirrored key.
        assert_eq!(
            get_repo_setting(&read, &id, DATA_POLICY_KEY).expect("raw"),
            Some(policy.as_str().to_string()),
        );
    }
}

#[tokio::test]
async fn unset_data_policy_reads_none() {
    let (_home, db) = open_state();
    let id = repo_id(5);
    create_repo(&db, &id).await;

    let read = db.open_read().expect("read conn");
    assert_eq!(repo_data_policy(&read, &id).expect("read policy"), None);
}

#[tokio::test]
async fn corrupt_stored_policy_is_typed_conversion_failure() {
    let (_home, db) = open_state();
    let id = repo_id(6);
    create_repo(&db, &id).await;
    // A value outside the canonical set (generic setter does not validate).
    set(&db, &id, DATA_POLICY_KEY, "definitely_not_a_policy").await;

    let read = db.open_read().expect("read conn");
    let result = repo_data_policy(&read, &id);
    assert!(
        matches!(result, Err(Error::FromSqlConversionFailure(0, _, _)),),
        "corrupt stored policy → typed conversion failure, got {result:?}",
    );
}

/// Every global×repo pair: the effective policy is the most restrictive of the
/// two ("every policy pair ordering" from the card).
#[tokio::test]
async fn effective_policy_is_most_restrictive_for_every_pair() {
    let (_home, db) = open_state();
    let id = repo_id(7);
    create_repo(&db, &id).await;

    for global in ALL_POLICIES {
        for repo in ALL_POLICIES {
            set_policy(&db, &id, repo).await;
            let read = db.open_read().expect("read conn");
            let effective = effective_data_policy(global, &read, &[&id]).expect("effective");
            assert_eq!(
                effective,
                global.most_restrictive(repo),
                "global {global:?} × repo {repo:?}",
            );
        }
    }
}

/// A repository can only *tighten* the global policy: a looser repo value never
/// relaxes a stricter global (spec 02 §3.2).
#[tokio::test]
async fn repo_cannot_relax_global() {
    let (_home, db) = open_state();
    let id = repo_id(8);
    create_repo(&db, &id).await;
    set_policy(&db, &id, DataPolicy::AllowRemoteFull).await; // loosest possible repo value

    let read = db.open_read().expect("read conn");
    let effective =
        effective_data_policy(DataPolicy::MetadataOnlyRemote, &read, &[&id]).expect("effective");
    assert_eq!(
        effective,
        DataPolicy::MetadataOnlyRemote,
        "the stricter global must win over a looser repo",
    );
}

/// A repository with no `data_policy` setting does not affect the effective
/// policy (it stays at the global value).
#[tokio::test]
async fn repo_without_setting_leaves_global() {
    let (_home, db) = open_state();
    let id = repo_id(9);
    create_repo(&db, &id).await;

    let read = db.open_read().expect("read conn");
    let effective =
        effective_data_policy(DataPolicy::AllowRemoteWithRedaction, &read, &[&id]).expect("eff");
    assert_eq!(effective, DataPolicy::AllowRemoteWithRedaction);
}

/// Across several repositories the effective policy is the strictest of all of
/// them and the global, and the result is independent of the order of `repo_ids`
/// (deterministic merged snapshot).
#[tokio::test]
async fn multi_repo_tightening_is_order_independent() {
    let (_home, db) = open_state();
    let (a, b, c) = (repo_id(10), repo_id(11), repo_id(12));
    for id in [&a, &b, &c] {
        create_repo(&db, id).await;
    }
    set_policy(&db, &a, DataPolicy::AllowRemoteFull).await;
    set_policy(&db, &b, DataPolicy::MetadataOnlyRemote).await; // strictest repo
    set_policy(&db, &c, DataPolicy::AllowRemoteWithRedaction).await;

    let read = db.open_read().expect("read conn");
    let global = DataPolicy::AllowRemoteFull;
    let forward = effective_data_policy(global, &read, &[&a, &b, &c]).expect("forward");
    let reverse = effective_data_policy(global, &read, &[&c, &b, &a]).expect("reverse");

    assert_eq!(
        forward,
        DataPolicy::MetadataOnlyRemote,
        "strictest of the set wins"
    );
    assert_eq!(
        forward, reverse,
        "merge is deterministic regardless of order"
    );
}

/// `repo_settings` lists all keys deterministically (ordered by key).
#[tokio::test]
async fn repo_settings_listing_is_ordered() {
    let (_home, db) = open_state();
    let id = repo_id(13);
    create_repo(&db, &id).await;
    set(&db, &id, "zeta", "1").await;
    set(&db, &id, "alpha", "2").await;
    set_policy(&db, &id, DataPolicy::LocalOnly).await;

    let read = db.open_read().expect("read conn");
    let all = repo_settings(&read, &id).expect("list");
    assert_eq!(
        all,
        vec![
            ("alpha".to_string(), "2".to_string()),
            (DATA_POLICY_KEY.to_string(), "local_only".to_string()),
            ("zeta".to_string(), "1".to_string()),
        ],
    );
}
