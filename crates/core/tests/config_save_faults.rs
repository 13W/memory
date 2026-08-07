//! T18-07's atomic-write crash test: `Config::save` must never leave a torn
//! `config.toml` on disk, driven by a named crash point rather than a timer —
//! the same convention as `crates/models/tests/install_faults.rs`.
//!
//! `config.save.between_write_and_rename` fires after the replacement text has
//! been written to `config.toml`'s `.tmp` sibling and before that sibling is
//! renamed into place — the exact instant a `kill -9` would otherwise be able to
//! interleave a reader with a half-written file. Renaming is a single filesystem
//! operation, so the only two observable outcomes for the *final* path are "old
//! content, untouched" (this test) and "new content, complete" (the ordinary
//! success path already covered in `crates/core/src/config/mod.rs`'s own tests).

#![cfg(feature = "failpoints")]

use std::fs;
use std::sync::Mutex;

use local_rag_core::DataPolicy;
use local_rag_core::config::{Config, config_toml_path};
use local_rag_test_support::TempHome;
use local_rag_test_support::failpoint::{Action, global};

const FAILPOINT: &str = "config.save.between_write_and_rename";

/// The failpoint registry is process-global, so an arming in one test would be
/// visible to a concurrently running one. Serializing the whole file is the same
/// remedy `crates/models/tests/install_faults.rs` uses.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Arm the crash point, guaranteeing it is disarmed when the guard drops — a
/// leaked arming would silently break every later test in the binary.
struct Armed;

impl Armed {
    fn new() -> Self {
        global().register(FAILPOINT);
        global().arm(FAILPOINT, Action::Error).expect("arm");
        Armed
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        let _ = global().disarm(FAILPOINT);
    }
}

#[test]
fn a_crash_between_write_and_rename_leaves_the_old_file_untouched() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let config_dir = home.join("config");
    fs::create_dir_all(&config_dir).expect("mk config dir");

    let old_text = "schema_version = 1\n\n[daemon]\nlog_level = \"warn\"\n";
    fs::write(config_toml_path(&config_dir), old_text).expect("seed old config.toml");

    let mut new_cfg = Config::default();
    new_cfg.daemon.log_level = "trace".to_string();
    new_cfg.models.data_policy = DataPolicy::AllowRemoteFull;

    let armed = Armed::new();
    new_cfg
        .save(&config_dir)
        .expect_err("the crash point fires");
    drop(armed);

    let on_disk = fs::read_to_string(config_toml_path(&config_dir)).expect("old file survives");
    assert_eq!(
        on_disk, old_text,
        "a crash between write and rename must not touch the previous file"
    );
    let reloaded = Config::load(&config_dir).expect("the untouched file still parses");
    assert_eq!(reloaded.daemon.log_level, "warn");
    assert_eq!(reloaded.models.data_policy, DataPolicy::LocalOnly);
}

#[test]
fn a_crash_on_a_first_ever_save_leaves_no_final_file() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let config_dir = home.join("config"); // never created, no prior config.toml

    let armed = Armed::new();
    Config::default()
        .save(&config_dir)
        .expect_err("the crash point fires");
    drop(armed);

    assert!(
        !config_toml_path(&config_dir).is_file(),
        "no config.toml must appear when the rename never happened"
    );
}

#[test]
fn a_retry_after_the_crash_succeeds_and_the_file_becomes_loadable() {
    let _serial = serial();
    let home = TempHome::new().expect("temp home");
    let config_dir = home.join("config");
    fs::create_dir_all(&config_dir).expect("mk config dir");

    let mut new_cfg = Config::default();
    new_cfg.daemon.log_level = "trace".to_string();

    let armed = Armed::new();
    new_cfg
        .save(&config_dir)
        .expect_err("the crash point fires");
    drop(armed);

    new_cfg.save(&config_dir).expect("the retry is not armed");
    let reloaded = Config::load(&config_dir).expect("load");
    assert_eq!(reloaded, new_cfg);
}
