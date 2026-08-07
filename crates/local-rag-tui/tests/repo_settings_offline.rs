//! T18-06 round-trip fixture tests: `execute_repo_settings_action`/`compute_repo_settings_data`
//! against a real `state.sqlite` — same per-file-fixture convention as `tests/memory_offline.rs`/
//! `tests/memory_mutations_offline.rs`, seed helpers duplicated from
//! `crates/local-rag/tests/support/mod.rs`. Only `create_repository`/`observe_repository_path` are
//! needed — `repo_settings` has no worktree dependency at all.
//!
//! Plain synchronous `#[test]`s, not `#[tokio::test]`, for the same reason
//! `memory_mutations_offline.rs` already established: `execute_repo_settings_action` drives its
//! own throwaway tokio runtime internally, and tokio forbids starting a runtime from inside a
//! runtime already driving the current thread. A local `block_on` drives only the async seed
//! calls, sequentially, never nested with `execute_repo_settings_action`'s own.

use std::future::Future;

use local_rag_core::DataPolicy;
use local_rag_core::paths::StoreLayout;
use local_rag_store::{
    StateDb, create_repository, observe_repository_path, repo_data_policy, repo_settings,
};
use local_rag_test_support::TempHome;
use local_rag_tui::repo_settings::{
    RepoSettingsAction, RepoSettingsNav, RepoSettingsScreenData, compute_repo_settings_data,
    execute_repo_settings_action,
};

fn block_on<F: Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread tokio runtime")
        .block_on(fut)
}

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

async fn seed_repo(layout: &StoreLayout, repo_id: &str, path: Option<&str>) {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let repo_id = repo_id.to_string();
    let path = path.map(|p| p.to_string());
    db.writer()
        .transaction(move |tx| {
            create_repository(tx, &repo_id, None, 1_000)?;
            if let Some(p) = &path {
                observe_repository_path(tx, &repo_id, p, 1_000)?;
            }
            Ok(())
        })
        .await
        .expect("seed repo tx");
}

fn ok_repo_detail(nav: RepoSettingsNav) -> (String, usize) {
    match nav {
        RepoSettingsNav::RepoDetail {
            repo_id,
            selected,
            error: None,
        } => (repo_id, selected),
        RepoSettingsNav::RepoDetail { error: Some(e), .. } => {
            panic!("expected success, got error: {e}")
        }
        other => panic!("expected RepoDetail, got {other:?}"),
    }
}

fn error_repo_detail(nav: RepoSettingsNav) -> String {
    match nav {
        RepoSettingsNav::RepoDetail { error: Some(e), .. } => e,
        RepoSettingsNav::RepoDetail { error: None, .. } => {
            panic!("expected an error, got success")
        }
        other => panic!("expected RepoDetail, got {other:?}"),
    }
}

// ---- 1: SetDataPolicy round-trip, upsert not duplicate ----

#[test]
fn set_data_policy_round_trips_and_upserts() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_repo(&layout, "repo-a", None));

    ok_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetDataPolicy {
            repo_id: "repo-a".to_string(),
            policy: DataPolicy::AllowRemoteWithRedaction,
            list_selected: 0,
        },
    ));

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    assert_eq!(
        repo_data_policy(&conn, "repo-a").expect("read data_policy"),
        Some(DataPolicy::AllowRemoteWithRedaction)
    );

    ok_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetDataPolicy {
            repo_id: "repo-a".to_string(),
            policy: DataPolicy::LocalOnly,
            list_selected: 0,
        },
    ));
    let conn = db.open_read().expect("read connection");
    assert_eq!(
        repo_data_policy(&conn, "repo-a").expect("read data_policy"),
        Some(DataPolicy::LocalOnly)
    );
    // Upsert, not a second row.
    assert_eq!(
        repo_settings(&conn, "repo-a").expect("list settings").len(),
        1
    );
}

// ---- 2: SetSetting round-trip, upsert not duplicate ----

#[test]
fn set_setting_round_trips_and_upserts() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_repo(&layout, "repo-a", None));

    ok_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetSetting {
            repo_id: "repo-a".to_string(),
            key: "default_model_space".to_string(),
            value: "fast".to_string(),
            list_selected: 0,
        },
    ));

    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let conn = db.open_read().expect("read connection");
    let rows = repo_settings(&conn, "repo-a").expect("list settings");
    assert_eq!(
        rows,
        vec![("default_model_space".to_string(), "fast".to_string())]
    );

    ok_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetSetting {
            repo_id: "repo-a".to_string(),
            key: "default_model_space".to_string(),
            value: "slow".to_string(),
            list_selected: 0,
        },
    ));
    let conn = db.open_read().expect("read connection");
    let rows = repo_settings(&conn, "repo-a").expect("list settings");
    assert_eq!(
        rows,
        vec![("default_model_space".to_string(), "slow".to_string())]
    );
}

// ---- 3: compute_repo_list shows every registered repository with its current_path ----

#[test]
fn compute_repo_list_shows_every_repo_with_current_path() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_repo(&layout, "repo-a", Some("/repos/a")));
    block_on(seed_repo(&layout, "repo-b", Some("/repos/b")));

    let data = compute_repo_settings_data(&layout, &RepoSettingsNav::RepoList { selected: 0 });
    match data {
        RepoSettingsScreenData::RepoList { rows, .. } => {
            assert_eq!(rows.len(), 2, "{rows:?}");
            let a = rows
                .iter()
                .find(|r| r.repo_id == "repo-a")
                .expect("repo-a present");
            assert_eq!(a.current_path.as_deref(), Some("/repos/a"));
            let b = rows
                .iter()
                .find(|r| r.repo_id == "repo-b")
                .expect("repo-b present");
            assert_eq!(b.current_path.as_deref(), Some("/repos/b"));
        }
        other => panic!("expected RepoList, got {other:?}"),
    }
}

// ---- 4: compute_repo_detail separates data_policy from the generic settings list ----

#[test]
fn compute_repo_detail_separates_data_policy_from_generic_settings() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    block_on(seed_repo(&layout, "repo-a", None));
    ok_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetDataPolicy {
            repo_id: "repo-a".to_string(),
            policy: DataPolicy::MetadataOnlyRemote,
            list_selected: 0,
        },
    ));
    ok_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetSetting {
            repo_id: "repo-a".to_string(),
            key: "alpha".to_string(),
            value: "1".to_string(),
            list_selected: 0,
        },
    ));
    ok_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetSetting {
            repo_id: "repo-a".to_string(),
            key: "beta".to_string(),
            value: "2".to_string(),
            list_selected: 0,
        },
    ));

    let data = compute_repo_settings_data(
        &layout,
        &RepoSettingsNav::RepoDetail {
            repo_id: "repo-a".to_string(),
            selected: 0,
            error: None,
        },
    );
    match data {
        RepoSettingsScreenData::RepoDetail {
            data_policy,
            settings,
            ..
        } => {
            assert_eq!(data_policy, Some(DataPolicy::MetadataOnlyRemote));
            assert_eq!(
                settings,
                vec![
                    ("alpha".to_string(), "1".to_string()),
                    ("beta".to_string(), "2".to_string()),
                ]
            );
        }
        other => panic!("expected RepoDetail, got {other:?}"),
    }
}

// ---- 5: writing to an unknown repo_id surfaces an inline error, not a panic ----

#[test]
fn writing_to_an_unknown_repo_id_surfaces_an_inline_error() {
    let (_home, layout) = open_layout();
    StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");
    // No seed_repo call — "repo-ghost" is never created.

    let error = error_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetSetting {
            repo_id: "repo-ghost".to_string(),
            key: "alpha".to_string(),
            value: "1".to_string(),
            list_selected: 0,
        },
    ));
    assert!(error.contains("could not save setting"), "{error}");
}

// ---- 6: write path refuses before the store is ever initialized ----

#[test]
fn write_path_refuses_before_the_store_is_ever_initialized() {
    let (_home, layout) = open_layout();
    // `layout.ensure()` only creates the directory tree — no `state.sqlite` exists yet.
    let error = error_repo_detail(execute_repo_settings_action(
        &layout,
        RepoSettingsAction::SetSetting {
            repo_id: "repo-a".to_string(),
            key: "alpha".to_string(),
            value: "1".to_string(),
            list_selected: 0,
        },
    ));
    assert!(error.contains("not yet initialized"), "{error}");
}

// ---- 7: read path is Unavailable before the store is ever initialized ----

#[test]
fn read_path_is_unavailable_before_the_store_is_ever_initialized() {
    let (_home, layout) = open_layout();
    let data = compute_repo_settings_data(&layout, &RepoSettingsNav::RepoList { selected: 0 });
    match data {
        RepoSettingsScreenData::Unavailable { reason } => {
            assert!(reason.contains("not yet initialized"), "{reason}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
