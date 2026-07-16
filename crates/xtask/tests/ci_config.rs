//! CI config lint (T00-02 acceptance).
//!
//! The checked-in CI workflow must run the single documented full-check
//! command on a single host, and the toolchain must be pinned. These are
//! plain text assertions over repo-relative files: deterministic, offline,
//! and independent of `$HOME`.

use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    // crates/xtask -> crates -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

#[test]
fn ci_workflow_runs_full_check_on_single_host() {
    let path = workspace_root().join(".github/workflows/ci.yml");
    let ci = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        ci.contains("cargo xtask ci"),
        "CI must invoke the single documented full-check command"
    );
    assert!(
        ci.contains("runs-on: ubuntu-latest"),
        "CI must target a single host"
    );
}

#[test]
fn toolchain_is_pinned() {
    let path = workspace_root().join("rust-toolchain.toml");
    let toolchain =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        toolchain.contains("channel = \"1.96.1\""),
        "toolchain channel must be pinned to 1.96.1"
    );
}
