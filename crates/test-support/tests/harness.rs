//! T00-03 acceptance tests for the shared harness.
//!
//! Deterministic and offline: no network, no real wall-clock, no `$HOME`.

use std::fs;

use local_rag_test_support::{
    Action, Clock, Failpoints, FixedClock, IdSource, ManualClock, SeqUuids, TempHome, run_capturing,
};

/// Two temporary homes created in parallel are fully isolated: distinct
/// paths under the exact same isolated temp root (D-023: not necessarily
/// `std::env::temp_dir()` itself — `TempHome` prefers the shorter `/tmp` on
/// Unix to keep derived Unix-domain-socket paths under `sockaddr_un.
/// sun_path`'s limit), and a write to one is invisible to the other.
#[test]
fn two_temp_homes_are_isolated() {
    let a = TempHome::new().expect("home a");
    let b = TempHome::new().expect("home b");
    assert_ne!(a.path(), b.path());
    assert_eq!(
        a.path().parent(),
        b.path().parent(),
        "both must live under the same isolated temp root"
    );

    fs::write(a.join("marker.txt"), b"in a").expect("write a");
    assert!(a.join("marker.txt").exists());
    assert!(
        !b.join("marker.txt").exists(),
        "home b must not see home a's file"
    );
}

/// A dropped home removes its directory.
#[test]
fn temp_home_is_removed_on_drop() {
    let home = TempHome::new().expect("home");
    let path = home.path().to_path_buf();
    assert!(path.is_dir());
    drop(home);
    assert!(!path.exists());
}

/// The temp home must never live under the user's real `$HOME`.
#[test]
fn temp_home_does_not_use_user_home() {
    let home = TempHome::new().expect("home");
    if let Some(user_home) = std::env::var_os("HOME") {
        let user_home = std::path::PathBuf::from(user_home);
        // An empty HOME would trivially pass; guard against that.
        if !user_home.as_os_str().is_empty() {
            assert!(
                !home.path().starts_with(&user_home),
                "temp home {} must not be under $HOME {}",
                home.path().display(),
                user_home.display()
            );
        }
    }
}

/// Fixed and manual clocks are reproducible: identical inputs, identical output.
#[test]
fn clocks_are_reproducible() {
    let fixed = FixedClock::new(1_234);
    assert_eq!(fixed.now_nanos(), 1_234);
    assert_eq!(fixed.now_nanos(), 1_234);

    let a = ManualClock::new(100);
    let b = ManualClock::new(100);
    for step in [1, 2, 3, 5, 8] {
        a.advance(step);
        b.advance(step);
        assert_eq!(a.now_nanos(), b.now_nanos());
    }
    a.set(0);
    assert_eq!(a.now_nanos(), 0);
}

/// Seeded id sources are reproducible and unique within a run.
#[test]
fn seeded_ids_are_reproducible() {
    let a = SeqUuids::seeded(42);
    let b = SeqUuids::seeded(42);
    let seq_a: Vec<String> = (0..4).map(|_| a.next_id()).collect();
    let seq_b: Vec<String> = (0..4).map(|_| b.next_id()).collect();
    assert_eq!(seq_a, seq_b, "same seed => same sequence");

    // Unique within one generator; different seed => different output.
    assert_eq!(
        seq_a.iter().collect::<std::collections::HashSet<_>>().len(),
        seq_a.len()
    );
    assert_ne!(SeqUuids::seeded(43).next_id(), seq_a[0]);
}

/// Arming an undeclared failpoint is rejected; a declared+armed one evaluates.
#[test]
fn unknown_failpoint_is_rejected() {
    let fp = Failpoints::new();
    fp.register("proj.write_ahead");
    assert!(fp.arm("proj.write_ahead", Action::Abort).is_ok());
    assert_eq!(fp.eval("proj.write_ahead").unwrap(), Some(Action::Abort));

    assert!(
        fp.arm("does.not.exist", Action::Abort).is_err(),
        "arming an undeclared failpoint must be rejected"
    );
    assert!(fp.eval("does.not.exist").is_err());
}

/// A crashing subprocess leaves an accessible artifact bundle with the captured
/// streams; the bundle lives outside any temp home, so it survives.
#[test]
fn subprocess_crash_leaves_artifact_bundle() {
    let cmd = {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_crash-helper"));
        c.arg("abort");
        c
    };
    let outcome = run_capturing(cmd, "crash-abort").expect("run helper");

    assert!(!outcome.success(), "helper must exit abnormally");
    let bundle = outcome
        .bundle
        .as_ref()
        .expect("crash must produce a bundle");
    assert!(
        bundle.is_dir(),
        "bundle dir must exist: {}",
        bundle.display()
    );

    for name in ["command.txt", "stdout.log", "stderr.log", "status.txt"] {
        let f = bundle.join(name);
        assert!(f.exists(), "bundle must contain {name}");
    }
    let captured_stdout = fs::read_to_string(bundle.join("stdout.log")).expect("read stdout.log");
    assert!(captured_stdout.contains("crash-helper: stdout marker"));
    assert!(
        outcome
            .stdout_lossy()
            .contains("crash-helper: stdout marker")
    );
}

/// A successful subprocess produces no bundle.
#[test]
fn subprocess_success_leaves_no_bundle() {
    let cmd = std::process::Command::new(env!("CARGO_BIN_EXE_crash-helper"));
    let outcome = run_capturing(cmd, "ok").expect("run helper");
    assert!(outcome.success());
    assert!(outcome.bundle.is_none(), "success must not write a bundle");
}
