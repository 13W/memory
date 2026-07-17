//! T02-05 acceptance tests for global-config **loading** from disk (spec 02
//! §3.1/§3.2).
//!
//! All tests are deterministic: an isolated [`TempHome`] under the system temp
//! dir, no network, no `$HOME`. The parse/validation matrix lives in the crate's
//! `--lib` unit tests; these cover the filesystem behavior of [`Config::load`]
//! and the "no repo-local config file lookup" invariant (spec 02 §3.2).

use std::fs;

use local_rag_core::config::{Config, config_toml_path};
use local_rag_core::{ConfigError, DataPolicy};
use local_rag_test_support::TempHome;

/// A missing `config.toml` yields the full spec defaults, not an error.
#[test]
fn missing_config_file_is_defaults() {
    let home = TempHome::new().expect("temp home");
    let config_dir = home.join("config");
    fs::create_dir_all(&config_dir).expect("mk config dir");

    let cfg = Config::load(&config_dir).expect("missing file → defaults");
    assert_eq!(cfg, Config::default());
    assert_eq!(cfg.models.data_policy, DataPolicy::LocalOnly);
}

/// A missing config **directory** is likewise just defaults (NotFound).
#[test]
fn missing_config_dir_is_defaults() {
    let home = TempHome::new().expect("temp home");
    let config_dir = home.join("config"); // never created
    let cfg = Config::load(&config_dir).expect("missing dir → defaults");
    assert_eq!(cfg, Config::default());
}

/// A present `config.toml` is parsed and validated.
#[test]
fn present_config_file_is_parsed() {
    let home = TempHome::new().expect("temp home");
    let config_dir = home.join("config");
    fs::create_dir_all(&config_dir).expect("mk config dir");
    fs::write(
        config_toml_path(&config_dir),
        "schema_version = 1\n\n[models]\ndata_policy = \"metadata_only_remote\"\ndefault_model_space = \"fast\"\n",
    )
    .expect("write config.toml");

    let cfg = Config::load(&config_dir).expect("valid file parses");
    assert_eq!(cfg.models.data_policy, DataPolicy::MetadataOnlyRemote);
    assert_eq!(cfg.models.default_model_space, "fast");
}

/// An invalid enum in a present file surfaces as a typed error (not defaults).
#[test]
fn present_but_invalid_config_is_typed_error() {
    let home = TempHome::new().expect("temp home");
    let config_dir = home.join("config");
    fs::create_dir_all(&config_dir).expect("mk config dir");
    fs::write(
        config_toml_path(&config_dir),
        "[models]\ndata_policy = \"send_it_all\"\n",
    )
    .expect("write config.toml");

    match Config::load(&config_dir) {
        Err(ConfigError::InvalidDataPolicy { value }) => assert_eq!(value, "send_it_all"),
        other => panic!("expected InvalidDataPolicy, got {other:?}"),
    }
}

/// **No repo-local config file lookup** (spec 02 §3.2): the loader consults only
/// `<config_dir>/config.toml`. A `config.toml` planted inside a repository
/// checkout — even nested under the very same home — has no effect, because the
/// single input to `Config::load` is `config_dir` and it never walks a worktree
/// tree. With no file at `<config_dir>/config.toml`, the result is defaults, and
/// crucially it is NOT the (deliberately dangerous) policy in the repo file.
#[test]
fn config_is_not_read_from_inside_a_repository() {
    let home = TempHome::new().expect("temp home");
    let config_dir = home.join("config");
    fs::create_dir_all(&config_dir).expect("mk config dir");

    // A repository checkout somewhere on disk carrying a config that tries to
    // loosen the daemon's policy to the most permissive value.
    let repo = home.join("work/some-repo");
    fs::create_dir_all(&repo).expect("mk repo dir");
    let planted = "schema_version = 1\n\n[models]\ndata_policy = \"allow_remote_full\"\n";
    fs::write(repo.join("config.toml"), planted).expect("plant repo config");
    fs::write(repo.join(".local-rag.toml"), planted).expect("plant dotfile config");

    // The loader looks only at <config_dir>/config.toml, which does not exist.
    let cfg = Config::load(&config_dir).expect("load ignores repo files");
    assert_eq!(cfg, Config::default());
    assert_eq!(
        cfg.models.data_policy,
        DataPolicy::LocalOnly,
        "a repo checkout must not be able to relax daemon policy",
    );
}
