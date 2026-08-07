//! T18-07 fixture tests: [`local_rag_tui::server_settings::initial_nav`]/
//! [`local_rag_tui::server_settings::execute_server_settings_action`] against a real `config.toml`
//! on disk — same per-file-fixture convention as `tests/repo_settings_offline.rs`, minus any tokio
//! runtime: unlike every other write screen, this one's mutation (`Config::save`) is a plain
//! synchronous file write, not a `StateWriter::transaction`.
//!
//! `config_dir` is built the same way `crates/core/tests/config.rs` builds one: a direct
//! `home.join("config")` path handed straight to `Config::load`/`Config::save`, never through
//! `local_rag_core::paths::config_dir`'s own `Env`-based resolution — `TempHome` is a plain
//! directory fixture, not an environment-variable stand-in, so there is nothing for that resolver
//! to consult here.

use std::fs;

use local_rag_core::DataPolicy;
use local_rag_core::config::config_toml_path;
use local_rag_test_support::TempHome;
use local_rag_tui::server_settings::{
    ServerSettingsAction, ServerSettingsNav, ServerSettingsScreenData,
    compute_server_settings_data, execute_server_settings_action, initial_nav,
};

fn config_dir(home: &TempHome) -> std::path::PathBuf {
    home.join("config")
}

#[test]
fn initial_nav_on_a_missing_file_is_defaults_with_no_status() {
    let home = TempHome::new().expect("temp home");
    let dir = config_dir(&home); // never created

    let nav = initial_nav(&dir);
    let ServerSettingsNav::FieldList {
        config,
        selected,
        status,
    } = nav
    else {
        panic!("expected FieldList");
    };
    assert_eq!(config, local_rag_core::config::Config::default());
    assert_eq!(selected, 0);
    assert_eq!(status, None);
}

#[test]
fn initial_nav_on_a_valid_file_loads_its_values() {
    let home = TempHome::new().expect("temp home");
    let dir = config_dir(&home);
    fs::create_dir_all(&dir).expect("mk config dir");
    fs::write(
        config_toml_path(&dir),
        "schema_version = 1\n\n[daemon]\nlog_level = \"debug\"\n\n[models]\ndata_policy = \"allow_remote_full\"\n",
    )
    .expect("write config.toml");

    let ServerSettingsNav::FieldList { config, status, .. } = initial_nav(&dir) else {
        panic!("expected FieldList");
    };
    assert_eq!(config.daemon.log_level, "debug");
    assert_eq!(config.models.data_policy, DataPolicy::AllowRemoteFull);
    assert_eq!(status, None);
}

#[test]
fn initial_nav_on_an_invalid_file_falls_back_to_defaults_with_a_status_message() {
    let home = TempHome::new().expect("temp home");
    let dir = config_dir(&home);
    fs::create_dir_all(&dir).expect("mk config dir");
    fs::write(
        config_toml_path(&dir),
        "[models]\ndata_policy = \"send_it_all\"\n",
    )
    .expect("write config.toml");

    let ServerSettingsNav::FieldList { config, status, .. } = initial_nav(&dir) else {
        panic!("expected FieldList");
    };
    assert_eq!(config, local_rag_core::config::Config::default());
    let status = status.expect("an explanatory status message");
    assert!(status.contains("send_it_all"), "{status}");
}

#[test]
fn execute_save_writes_a_real_config_toml_readable_back_through_load() {
    let home = TempHome::new().expect("temp home");
    let dir = config_dir(&home);

    let mut config = local_rag_core::config::Config::default();
    config.daemon.log_level = "trace".to_string();
    config.models.data_policy = DataPolicy::MetadataOnlyRemote;

    let nav = execute_server_settings_action(
        &dir,
        ServerSettingsAction::Save {
            config: config.clone(),
            selected: 2,
        },
    );

    match nav {
        ServerSettingsNav::SavedPrompt {
            config: saved,
            selected,
        } => {
            assert_eq!(saved, config);
            assert_eq!(selected, 2);
        }
        other => panic!("expected SavedPrompt, got {other:?}"),
    }

    let reloaded = local_rag_core::config::Config::load(&dir).expect("load the saved file");
    assert_eq!(reloaded, config);
}

#[test]
fn compute_field_list_reflects_a_loaded_config() {
    let home = TempHome::new().expect("temp home");
    let dir = config_dir(&home);
    fs::create_dir_all(&dir).expect("mk config dir");
    fs::write(config_toml_path(&dir), "[daemon]\nmax_open_shards = 3\n")
        .expect("write config.toml");

    let nav = initial_nav(&dir);
    let ServerSettingsScreenData::FieldList { rows, .. } = compute_server_settings_data(&nav)
    else {
        panic!("expected FieldList data");
    };
    let row = rows
        .iter()
        .find(|(label, _)| label == "daemon.max_open_shards")
        .expect("daemon.max_open_shards row present");
    assert_eq!(row.1, "3");
}
