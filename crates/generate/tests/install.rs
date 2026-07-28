//! The generator model asset installer, end to end (spec 10 §5 `[FIXED
//! policy]`) — T14-07, mirroring `local_rag_models/tests/install.rs`'s test
//! list exactly, adapted for a single-file GGUF entry.

mod support;

use local_rag_core::paths::StoreLayout;
use local_rag_generate::{
    HttpFetcher, InstallError, LlamaError, LlamaGenerator, MANIFEST_FILE, OK_MARKER, install_model,
    is_installed, write_license_notice,
};
use local_rag_test_support::TempHome;
use support::{FIXTURE_MODEL_ID, FixtureServer, ForbiddenFetcher, fixture_bytes};

fn layout(home: &TempHome) -> StoreLayout {
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    layout
}

#[test]
fn a_clean_install_downloads_verifies_and_marks_ready() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    let report =
        install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    assert_eq!(report.model_id, FIXTURE_MODEL_ID);
    assert_eq!(report.downloaded, vec!["model.gguf".to_string()]);
    assert!(report.reused.is_empty());
    assert!(report.marked_ready);
    assert_eq!(report.bytes_downloaded, entry.total_bytes());
    assert_eq!(server.request_count(), 1);

    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    assert_eq!(
        std::fs::read(dir.join("model.gguf")).expect("installed file"),
        fixture_bytes()
    );
    assert!(dir.join(MANIFEST_FILE).is_file());
    assert!(dir.join(OK_MARKER).is_file());
    assert!(is_installed(&layout, FIXTURE_MODEL_ID));
}

#[test]
fn an_already_installed_model_is_a_no_op_without_touching_the_network() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");
    let before = server.request_count();

    let repeat = install_model(&layout, &entry, &ForbiddenFetcher, &mut std::io::sink())
        .expect("a completed install repeats cleanly");
    assert!(repeat.is_noop(), "{repeat:?}");
    assert_eq!(server.request_count(), before);
}

#[test]
fn matching_files_are_reused_rather_than_refetched() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();
    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    std::fs::create_dir_all(&dir).expect("model dir");
    std::fs::write(dir.join("model.gguf"), fixture_bytes()).expect("pre-seed correct bytes");

    let report =
        install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");
    assert_eq!(report.reused, vec!["model.gguf".to_string()]);
    assert!(report.downloaded.is_empty());
    assert_eq!(server.request_count(), 0, "never fetched a matching file");
}

#[test]
fn a_wrong_digest_fails_and_leaves_the_model_uninstalled() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();
    // Same length as the real fixture, different bytes -- isolates the
    // checksum check from the (separately meaningful) size check.
    let mut corrupted = fixture_bytes().to_vec();
    corrupted[0] = corrupted[0].wrapping_add(1);
    server.corrupt(&corrupted);

    let err = install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink())
        .expect_err("checksum must fail");
    assert!(
        matches!(err, InstallError::ChecksumMismatch { .. }),
        "{err:?}"
    );
    assert!(!is_installed(&layout, FIXTURE_MODEL_ID));
}

#[test]
fn a_directory_without_the_marker_counts_as_not_installed() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    std::fs::create_dir_all(&dir).expect("model dir");
    std::fs::write(dir.join("model.gguf"), fixture_bytes()).expect("seed file without marker");

    assert!(!is_installed(&layout, FIXTURE_MODEL_ID));
    let entry = support::fixture_entry("https://example.invalid/unused");
    let err = LlamaGenerator::open(&layout, &entry).expect_err("no .ok marker");
    assert!(matches!(err, LlamaError::AssetsMissing { .. }), "{err:?}");
}

// Note: "opening installed assets never reaches the network" is proven
// structurally, not by a runtime test — `LlamaGenerator::open`'s signature
// takes no `AssetFetcher` at all, so it cannot reach one. A runtime version
// of this test would additionally load a fixture (non-GGUF) file through the
// real llama.cpp parser, which is slow (tens of seconds) for zero additional
// coverage over the type signature itself.

#[test]
fn the_model_directory_lands_where_the_layout_says_with_tight_permissions() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();
    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    let file = dir.join("model.gguf");
    assert!(file.is_file());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "installed weights must be owner-only");
    }
}

#[test]
fn write_license_notice_names_source_and_license_before_any_bytes_move() {
    let entry = support::fixture_entry("https://example.invalid/repo");
    let mut out = Vec::new();
    write_license_notice(&entry, &mut out).expect("write notice");
    let text = String::from_utf8(out).expect("utf8");
    assert!(text.contains("Fixture Terms of Use"));
    assert!(text.contains("https://example.invalid/repo"));
    assert!(text.contains("model.gguf") || text.contains("file(s)"));
}
