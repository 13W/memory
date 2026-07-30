//! CLI smoke test: the binary answers `version` and rejects unknown commands.

use std::process::Command;

#[test]
fn version_subcommand_prints_name_and_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_local-rag"))
        .arg("version")
        .output()
        .expect("run local-rag");
    assert!(output.status.success(), "`version` must exit 0");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    assert_eq!(
        stdout.trim(),
        format!("local-rag {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_subcommand_exits_nonzero() {
    let output = Command::new(env!("CARGO_BIN_EXE_local-rag"))
        .arg("bogus")
        .output()
        .expect("run local-rag");
    assert!(!output.status.success(), "unknown subcommand must fail");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn no_arguments_at_all_is_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_local-rag"))
        .output()
        .expect("run local-rag");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage:"),
        "{:?}",
        output.stderr
    );
}
