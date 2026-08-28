//! `install_ort` — the ONNX Runtime as an artifact of first run (spec 10 §5
//! `[FIXED, ADR-0013]`, T22-15).
//!
//! | card bullet | test |
//! | --- | --- |
//! | archive digest matches / does not | [`a_clean_install_verifies_extracts_and_marks_ready`], [`a_wrong_archive_digest_fails_and_installs_nothing`] |
//! | extracted digest matches / does not | [`a_wrong_library_digest_fails_and_installs_nothing`] |
//! | already installed | [`an_installed_runtime_is_a_no_op_without_touching_the_fetcher`] |
//! | resumable | [`a_run_interrupted_after_the_download_finishes_without_refetching`], [`a_directory_without_the_marker_counts_as_not_installed`] |
//!
//! The fixture archive is built here rather than downloaded: the real assets
//! are 8–77 MB and need the network. What the real ones assert is the pins in
//! `ort_catalog.rs`, verified once by extracting all five and comparing
//! digests — see T22-15's evidence.

use std::path::Path;

mod support;

use local_rag_core::paths::StoreLayout;
use local_rag_models::{
    LocalFetcher, MANIFEST_FILE, OK_MARKER, OrtManifest, install_ort, ort_dylib_path,
    ort_is_installed,
};
use local_rag_test_support::TempHome;
use support::{ForbiddenFetcher, leak, ort_fixture as fixture};

fn layout(home: &TempHome) -> StoreLayout {
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    layout
}

#[test]
fn a_clean_install_verifies_extracts_and_marks_ready() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let payload = vec![42u8; 9000];
    let f = fixture(&home, &payload);

    assert!(!ort_is_installed(&layout, &f.asset));
    let report = install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect("install");

    assert!(report.marked_ready);
    assert_eq!(report.bytes_downloaded, f.asset.archive_size);
    assert!(ort_is_installed(&layout, &f.asset));

    let dylib = ort_dylib_path(&layout, &f.asset);
    assert_eq!(std::fs::read(&dylib).expect("read installed"), payload);
    // Under `models/`, keyed by version — "beside the weights" (10 §5).
    assert!(
        dylib.starts_with(layout.models_dir().join("onnxruntime").join("9.9.9")),
        "{}",
        dylib.display()
    );

    let dir = dylib.parent().expect("parent");
    let manifest: OrtManifest = OrtManifest::from_json(
        &std::fs::read_to_string(dir.join(MANIFEST_FILE)).expect("manifest"),
    )
    .expect("parse");
    assert_eq!(manifest.sha256, f.asset.dylib_sha256);
    assert_eq!(manifest.archive_sha256, f.asset.archive_sha256);
    assert_eq!(manifest.size, payload.len() as u64);

    // The archive was scaffolding and is gone; the marker is what remains.
    let left: Vec<String> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !left.iter().any(|n| n.ends_with(".tgz")),
        "archive kept: {left:?}"
    );
    assert!(left.iter().any(|n| n == OK_MARKER));
}

#[test]
fn an_installed_runtime_is_a_no_op_without_touching_the_fetcher() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let f = fixture(&home, b"payload");
    install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect("first install");

    let again = install_ort(&layout, &f.asset, &ForbiddenFetcher).expect("second install");
    assert!(!again.marked_ready);
    assert_eq!(again.bytes_downloaded, 0);
}

#[test]
fn a_wrong_archive_digest_fails_and_installs_nothing() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let mut f = fixture(&home, b"payload");
    f.asset.archive_sha256 = leak("0".repeat(64));

    let err = install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect_err("bad pin");
    assert!(format!("{err}").contains("checksum"), "{err}");
    assert!(!ort_is_installed(&layout, &f.asset));
    assert!(!ort_dylib_path(&layout, &f.asset).exists());
}

#[test]
fn a_wrong_library_digest_fails_and_installs_nothing() {
    // The check the archive pin cannot make. This is what catches this
    // project's own extractor putting the wrong member somewhere: the bytes on
    // the wire were exactly right, and the file taken out of them was not.
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let mut f = fixture(&home, b"payload");
    f.asset.dylib_sha256 = leak("1".repeat(64));

    let err = install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect_err("bad pin");
    let text = format!("{err}");
    assert!(text.contains("checksum"), "{text}");
    assert!(text.contains("libonnxruntime.test"), "{text}");
    assert!(!ort_is_installed(&layout, &f.asset));
    assert!(!ort_dylib_path(&layout, &f.asset).exists());
}

#[test]
fn a_member_the_archive_does_not_have_is_a_typed_error() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let mut f = fixture(&home, b"payload");
    f.asset.archive_member = "ort/lib/absent";

    let err = install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect_err("no member");
    assert!(format!("{err}").contains("no member named"), "{err}");
    assert!(!ort_is_installed(&layout, &f.asset));
}

#[test]
fn a_run_interrupted_after_the_download_finishes_without_refetching() {
    // The archive is kept until the marker is written precisely so this is
    // cheap: a 77 MB download must not be repeated because an extract was
    // interrupted. Simulated by installing, then removing everything the
    // install produced *except* the archive.
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let f = fixture(&home, b"payload");
    install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect("first install");

    let dir = ort_dylib_path(&layout, &f.asset)
        .parent()
        .expect("parent")
        .to_path_buf();
    for name in [OK_MARKER, MANIFEST_FILE, f.asset.dylib_name] {
        std::fs::remove_file(dir.join(name)).expect("undo the install");
    }
    std::fs::copy(
        f.served.join("ort-fixture.tgz"),
        dir.join("ort-fixture.tgz"),
    )
    .expect("archive");

    let report = install_ort(&layout, &f.asset, &ForbiddenFetcher).expect("resume");
    assert!(report.marked_ready);
    assert_eq!(
        report.bytes_downloaded, 0,
        "a kept archive must not be refetched"
    );
}

#[test]
fn a_leftover_archive_with_the_wrong_bytes_is_refetched_not_trusted() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let f = fixture(&home, b"payload");
    let dir = layout
        .models_dir()
        .join("onnxruntime")
        .join(f.asset.version);
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("ort-fixture.tgz"), b"not the archive").expect("decoy");

    let report = install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect("install");
    assert_eq!(report.bytes_downloaded, f.asset.archive_size);
    assert!(ort_is_installed(&layout, &f.asset));
}

#[test]
fn a_directory_without_the_marker_counts_as_not_installed() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let f = fixture(&home, b"payload");
    install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect("install");

    let dir = ort_dylib_path(&layout, &f.asset)
        .parent()
        .expect("parent")
        .to_path_buf();
    std::fs::remove_file(dir.join(OK_MARKER)).expect("remove marker");
    assert!(!ort_is_installed(&layout, &f.asset));
}

#[test]
fn the_installed_library_is_locked_down_like_every_other_store_file() {
    // Spec 02 §2.1 / 12 §6 `[FIXED]`. `dlopen` needs read, not execute, so 0600
    // is both sufficient and what the rest of the store already uses.
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let f = fixture(&home, b"payload");
    install_ort(&layout, &f.asset, &LocalFetcher::new(&f.served)).expect("install");

    assert_mode_0600(&ort_dylib_path(&layout, &f.asset));

    // And both directories, one level at a time. `perms::ensure_dir` builds a
    // single level precisely so each one gets 0700 and an ownership check; a
    // `create_dir_all` here would leave them at whatever the umask says, and
    // nothing else in this suite would notice.
    let version_dir = ort_dylib_path(&layout, &f.asset)
        .parent()
        .expect("parent")
        .to_path_buf();
    assert_mode_0700(&version_dir);
    assert_mode_0700(version_dir.parent().expect("onnxruntime dir"));
}

#[cfg(unix)]
fn assert_mode_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{} is {mode:o}", path.display());
}

#[cfg(unix)]
fn assert_mode_0700(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "{} is {mode:o}", path.display());
}

#[cfg(not(unix))]
fn assert_mode_0600(_path: &Path) {}

#[cfg(not(unix))]
fn assert_mode_0700(_path: &Path) {}
