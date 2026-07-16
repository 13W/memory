//! Acceptance smoke test: the shared harness (T00-03) drives the real
//! `local-rag` binary in an isolated temp home and captures its output.

use local_rag_test_support::{TempHome, run_capturing};

#[test]
fn harness_runs_local_rag_version_in_temp_home() {
    let home = TempHome::new().expect("temp home");
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.arg("version");

    let outcome = run_capturing(cmd, "local-rag-version").expect("run local-rag");

    assert!(outcome.success(), "`version` must exit 0");
    assert!(outcome.bundle.is_none(), "success writes no bundle");
    assert_eq!(
        outcome.stdout_lossy().trim(),
        format!("local-rag {}", env!("CARGO_PKG_VERSION"))
    );
}
