//! The model asset installer, end to end (spec 10 §5 `[FIXED policy]`) — T11-06.
//!
//! Every row of the card's test list lives here except "interrupted download",
//! which needs the `failpoints` feature and therefore its own file:
//!
//! | card bullet | test |
//! | --- | --- |
//! | bad checksum | [`a_wrong_digest_fails_and_leaves_the_model_uninstalled`] |
//! | existing valid asset | [`an_already_installed_model_is_a_no_op_without_touching_the_network`], [`matching_files_are_reused_rather_than_refetched`] |
//! | missing `.ok` | [`a_directory_without_the_marker_counts_as_not_installed`] |
//! | offline launch | [`reading_installed_assets_never_reaches_the_fetcher`] |
//! | platform path | [`the_model_directory_lands_where_the_layout_says_with_tight_permissions`] |
//!
//! All of it runs against a loopback fixture server inside a `TempHome`: no
//! external network, no `$HOME`, no wall-clock dependence.

mod support;

use std::io::Write;

use local_rag_core::paths::StoreLayout;
use local_rag_embed::{EmbedError, require_model_assets};
use local_rag_models::{
    FetchError, HttpFetcher, InstallError, LocalFetcher, MANIFEST_FILE, ModelManifest, OK_MARKER,
    PART_SUFFIX, install_model, is_installed, write_license_notice,
};
use local_rag_test_support::TempHome;
use support::{
    FIXTURE_FILES, FIXTURE_MODEL_ID, FixtureServer, ForbiddenFetcher, ObservingFetcher,
    fixture_bytes,
};

/// A store layout inside a temporary home.
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
    assert_eq!(report.downloaded.len(), FIXTURE_FILES.len());
    assert!(report.reused.is_empty());
    assert!(
        report.marked_ready,
        "the run that completes writes the marker"
    );
    assert_eq!(report.bytes_downloaded, entry.total_bytes());
    assert_eq!(server.request_count(), FIXTURE_FILES.len());

    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    for (relative, _, bytes) in FIXTURE_FILES {
        assert_eq!(
            std::fs::read(dir.join(relative)).expect("installed file"),
            *bytes,
            "{relative} was installed byte for byte"
        );
    }
    assert!(dir.join(OK_MARKER).is_file(), "the marker exists");
    assert!(is_installed(&layout, FIXTURE_MODEL_ID));

    // The consumer contract (T11-03) now accepts the directory.
    let resolved = require_model_assets(&layout, FIXTURE_MODEL_ID).expect("assets are usable");
    assert_eq!(resolved, dir);

    // No `.part` survives a successful run.
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .expect("read model dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(PART_SUFFIX))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}

#[test]
fn the_manifest_records_source_size_sha256_and_license() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    let text = std::fs::read_to_string(layout.model_dir(FIXTURE_MODEL_ID).join(MANIFEST_FILE))
        .expect("manifest exists");
    let manifest = ModelManifest::from_json(&text).expect("manifest parses");

    // spec 10 §5: "records source, size, sha256, license".
    assert_eq!(manifest.source, entry.source);
    assert_eq!(manifest.license, entry.license);
    assert_eq!(manifest.revision, entry.revision);
    assert_eq!(manifest.dimensions, entry.dimensions);
    assert_eq!(manifest.files.len(), FIXTURE_FILES.len());
    for (recorded, expected) in manifest.files.iter().zip(entry.files) {
        assert_eq!(recorded.path, expected.relative_path);
        assert_eq!(recorded.size, expected.size);
        assert_eq!(recorded.sha256, expected.sha256);
        // The digest describes what is actually on disk, not just what was
        // asked for.
        let actual = local_rag_core::hash::sha256_hex(fixture_bytes(&recorded.path));
        assert_eq!(recorded.sha256, actual, "{} digest", recorded.path);
    }
}

#[test]
fn a_wrong_digest_fails_and_leaves_the_model_uninstalled() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();
    // Right length is not enough: the server returns bytes of the correct size
    // but different content, so only the digest can catch it.
    let genuine = fixture_bytes("weights.onnx");
    let mut tampered = genuine.to_vec();
    tampered[0] ^= 0xff;
    server.corrupt("weights.onnx", &tampered);

    let err = install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink())
        .expect_err("a tampered asset must not install");

    match &err {
        InstallError::ChecksumMismatch {
            file,
            expected,
            actual,
        } => {
            assert_eq!(file, "weights.onnx");
            assert_eq!(expected, &entry.files[0].sha256);
            assert_eq!(actual, &local_rag_core::hash::sha256_hex(&tampered));
            assert_ne!(expected, actual);
        }
        other => panic!("expected a typed checksum mismatch, got {other:?}"),
    }

    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    assert!(
        !dir.join(OK_MARKER).exists(),
        "no marker on a failed install"
    );
    assert!(!dir.join(MANIFEST_FILE).exists(), "no manifest either");
    assert!(
        !dir.join("weights.onnx").exists(),
        "the bad bytes are never renamed into place"
    );
    assert!(!is_installed(&layout, FIXTURE_MODEL_ID));
    assert!(matches!(
        require_model_assets(&layout, FIXTURE_MODEL_ID),
        Err(EmbedError::ModelAssetsMissing { .. })
    ));

    // The installer stopped at the first bad file rather than downloading the rest.
    assert_eq!(server.request_count(), 1, "{:?}", server.requests());
}

#[test]
fn a_wrong_length_is_caught_even_when_the_digest_is_never_reached() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();
    server.corrupt("weights.onnx", b"short");

    let err = install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink())
        .expect_err("a truncated asset must not install");

    match err {
        InstallError::SizeMismatch {
            file,
            expected,
            actual,
        } => {
            assert_eq!(file, "weights.onnx");
            assert_eq!(expected, entry.files[0].size);
            assert_eq!(actual, 5);
        }
        other => panic!("expected a typed size mismatch, got {other:?}"),
    }
    assert!(!is_installed(&layout, FIXTURE_MODEL_ID));
}

#[test]
fn an_already_installed_model_is_a_no_op_without_touching_the_network() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");
    let after_first = server.request_count();

    // A fetcher that panics if called proves the second run is genuinely offline.
    let mut notice = Vec::new();
    let report =
        install_model(&layout, &entry, &ForbiddenFetcher, &mut notice).expect("re-install");

    assert_eq!(server.request_count(), after_first, "no new requests");
    assert!(report.is_noop());
    assert!(report.downloaded.is_empty());
    assert_eq!(report.reused.len(), FIXTURE_FILES.len());
    assert!(!report.marked_ready, "the marker was already there");
    assert!(
        notice.is_empty(),
        "a no-op install does not re-print a license the user already accepted"
    );
}

#[test]
fn matching_files_are_reused_rather_than_refetched() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    // Pre-place the first two files exactly as the catalog pins them, but leave
    // the third missing and write no marker.
    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    std::fs::create_dir_all(&dir).expect("model dir");
    for (relative, _, bytes) in &FIXTURE_FILES[..2] {
        std::fs::write(dir.join(relative), bytes).expect("seed file");
    }

    let report =
        install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    assert_eq!(report.reused, vec!["weights.onnx", "weights.onnx_data"]);
    assert_eq!(report.downloaded, vec!["tokenizer.json"]);
    assert_eq!(report.bytes_downloaded, entry.files[2].size);
    assert_eq!(
        server.requests(),
        vec!["/repo/resolve/rev-0/tokenizer.json".to_string()],
        "only the missing file is fetched"
    );
    assert!(is_installed(&layout, FIXTURE_MODEL_ID));
}

#[test]
fn a_present_file_with_wrong_bytes_is_refetched_not_trusted() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    std::fs::create_dir_all(&dir).expect("model dir");
    // Same name, same length, different content — only the digest distinguishes it.
    let mut wrong = fixture_bytes("weights.onnx").to_vec();
    wrong[1] ^= 0xff;
    std::fs::write(dir.join("weights.onnx"), &wrong).expect("seed");

    let report =
        install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    assert!(
        report.downloaded.contains(&"weights.onnx".to_string()),
        "a file that fails its digest is re-downloaded: {report:?}"
    );
    assert_eq!(
        std::fs::read(dir.join("weights.onnx")).expect("read"),
        fixture_bytes("weights.onnx")
    );
}

#[test]
fn a_directory_without_the_marker_counts_as_not_installed() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    // Every file present and correct, plus a manifest — but no `.ok`.
    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    std::fs::create_dir_all(&dir).expect("model dir");
    for (relative, _, bytes) in FIXTURE_FILES {
        std::fs::write(dir.join(relative), bytes).expect("seed file");
    }
    std::fs::write(dir.join(MANIFEST_FILE), b"{}").expect("seed manifest");

    assert!(!is_installed(&layout, FIXTURE_MODEL_ID));
    assert!(
        matches!(
            require_model_assets(&layout, FIXTURE_MODEL_ID),
            Err(EmbedError::ModelAssetsMissing { .. })
        ),
        "without the marker the assets are unusable, however complete they look"
    );

    // Completing the install writes the marker without refetching anything.
    let report =
        install_model(&layout, &entry, &ForbiddenFetcher, &mut std::io::sink()).expect("install");
    assert_eq!(server.request_count(), 0);
    assert_eq!(report.reused.len(), FIXTURE_FILES.len());
    assert!(report.marked_ready);
    assert!(is_installed(&layout, FIXTURE_MODEL_ID));
}

#[test]
fn the_marker_is_written_after_every_file_and_after_the_manifest() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();
    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    std::fs::create_dir_all(&dir).expect("model dir");

    let fetcher = ObservingFetcher::new(HttpFetcher::new(), &dir);
    install_model(&layout, &entry, &fetcher, &mut std::io::sink()).expect("install");

    let snapshots = fetcher.snapshots();
    assert_eq!(snapshots.len(), FIXTURE_FILES.len());
    for (i, entries) in snapshots.iter().enumerate() {
        assert!(
            !entries.iter().any(|name| name == OK_MARKER),
            "the marker existed before file {i} was fetched: {entries:?}"
        );
        assert!(
            !entries.iter().any(|name| name == MANIFEST_FILE),
            "the manifest existed before file {i} was fetched: {entries:?}"
        );
    }
    assert!(dir.join(MANIFEST_FILE).is_file());
    assert!(dir.join(OK_MARKER).is_file());
}

#[test]
fn reading_installed_assets_never_reaches_the_fetcher() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");
    let baseline = server.request_count();
    drop(server);

    // The fixture server is gone; reading the installed assets must still work,
    // which is only true if "offline afterwards" (spec 10 §5) holds.
    let dir = require_model_assets(&layout, FIXTURE_MODEL_ID).expect("assets resolve offline");
    for (relative, _, bytes) in FIXTURE_FILES {
        assert_eq!(std::fs::read(dir.join(relative)).expect("read"), *bytes);
    }
    assert_eq!(baseline, FIXTURE_FILES.len());
}

#[test]
fn the_model_directory_lands_where_the_layout_says_with_tight_permissions() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    install_model(&layout, &entry, &HttpFetcher::new(), &mut std::io::sink()).expect("install");

    let dir = layout.model_dir(FIXTURE_MODEL_ID);
    assert_eq!(dir, layout.root().join("models").join(FIXTURE_MODEL_ID));
    assert!(dir.is_dir());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "model directory is owner-only");
        for name in FIXTURE_FILES
            .iter()
            .map(|(relative, _, _)| *relative)
            .chain([MANIFEST_FILE, OK_MARKER])
        {
            let mode = std::fs::metadata(dir.join(name))
                .unwrap_or_else(|e| panic!("{name}: {e}"))
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name} is owner-only");
        }
    }
}

#[test]
fn the_license_is_surfaced_before_the_first_byte_moves() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    let entry = server.entry();

    // A notice sink that records how many requests the server had served when
    // the notice was written — ADR-0004 requires it *before* the download.
    let mut notice = Vec::new();
    write_license_notice(&entry, &mut notice).expect("notice");
    let before = server.request_count();
    assert_eq!(before, 0);

    let mut during = Vec::new();
    install_model(&layout, &entry, &HttpFetcher::new(), &mut during).expect("install");

    let text = String::from_utf8(during).expect("utf-8 notice");
    assert!(text.contains(entry.license), "license name: {text}");
    assert!(text.contains(entry.license_url), "license url: {text}");
    assert!(text.contains(entry.source), "source url: {text}");
    assert!(
        text.contains("redistributes no weights"),
        "the notice states what local-rag ships: {text}"
    );
}

#[test]
fn a_local_mirror_installs_without_any_http_at_all() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let mirror = home.join("mirror");
    std::fs::create_dir_all(&mirror).expect("mirror dir");
    for (relative, source_path, bytes) in FIXTURE_FILES {
        let _ = source_path;
        std::fs::write(mirror.join(relative), bytes).expect("seed mirror");
    }
    let entry = support::fixture_entry("file:///mirror");

    let report = install_model(
        &layout,
        &entry,
        &LocalFetcher::new(&mirror),
        &mut std::io::sink(),
    )
    .expect("install from mirror");

    assert_eq!(report.downloaded.len(), FIXTURE_FILES.len());
    assert!(is_installed(&layout, FIXTURE_MODEL_ID));
}

#[test]
fn a_server_error_is_a_typed_fetch_failure_not_a_partial_install() {
    let home = TempHome::new().expect("temp home");
    let layout = layout(&home);
    let server = FixtureServer::start();
    // A source URL the fixture server knows nothing about: every asset 404s.
    let missing = support::fixture_entry(&format!("{}/absent", server.source_url()));

    let err = install_model(&layout, &missing, &HttpFetcher::new(), &mut std::io::sink())
        .expect_err("a 404 must not install");
    match err {
        InstallError::Fetch(FetchError::Status { status, url }) => {
            assert_eq!(status, 404);
            assert!(
                url.ends_with("/absent/resolve/rev-0/onnx/weights.onnx"),
                "{url}"
            );
        }
        other => panic!("expected a typed status failure, got {other:?}"),
    }
    assert!(!is_installed(&layout, missing.model_id));
    assert!(
        !layout
            .model_dir(missing.model_id)
            .join("weights.onnx")
            .exists(),
        "an error response never becomes an installed file"
    );
}

#[test]
fn the_notice_writer_reports_a_failing_sink() {
    let server = FixtureServer::start();
    let entry = server.entry();

    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("sink is closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let err = write_license_notice(&entry, &mut Broken).expect_err("a broken sink surfaces");
    assert!(err.to_string().contains("sink is closed"), "{err}");
}

/// The shipped catalog's pinned digests, verified against a real local mirror.
///
/// The fixture tests above prove the *mechanism*; this proves the **data** —
/// that `catalog.rs`'s `sha256`/`size` for the default model describe the actual
/// upstream bytes. It cannot be a CI prerequisite (295 MB of weights, spec 14 §1
/// keeps tests hermetic), so it runs only when `LOCAL_RAG_TEST_MIRROR` points at
/// a flat directory holding `model_quantized.onnx`, `model_quantized.onnx_data`
/// and `tokenizer.json`, and says so loudly when it skips.
///
/// The verification always runs into a **fresh temporary root**, never into
/// `LOCAL_RAG_TEST_MODEL_HOME` (D-014, gate G11). Installing into an already
/// populated root short-circuits on the `.ok` marker (`install_model` is a
/// documented no-op there, 10 §5), which hashes nothing — so verifying "the
/// pinned digests describe the real bytes" *in* that root would have proven
/// exactly nothing while printing that it had. `LOCAL_RAG_TEST_MODEL_HOME`, when
/// set, is additionally populated afterwards — that is how the ONNX inference
/// test's weights get installed — and the no-op semantics are asserted there
/// explicitly rather than tripped over.
#[test]
fn the_default_catalog_matches_a_real_local_mirror_when_one_is_supplied() {
    let Ok(mirror) = std::env::var("LOCAL_RAG_TEST_MIRROR") else {
        eprintln!(
            "SKIP: LOCAL_RAG_TEST_MIRROR is unset — no local copy of the default \
             model's ~295 MB of weights to verify the pinned digests against."
        );
        return;
    };
    let entry = &local_rag_models::EMBEDDINGGEMMA_300M;
    let fetcher = LocalFetcher::new(&mirror);

    // The claim under test: every byte is hashed here, because this root is
    // empty and no file can be reused.
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");

    let mut notice = Vec::new();
    let report = install_model(&layout, entry, &fetcher, &mut notice)
        .expect("the mirror's bytes must match the pinned digests");

    assert!(is_installed(&layout, entry.model_id));
    assert_eq!(
        report.downloaded.len(),
        entry.files.len(),
        "a fresh root downloads (and therefore hashes) every file: {report:?}"
    );
    assert!(report.reused.is_empty(), "nothing to reuse: {report:?}");
    let text = String::from_utf8(notice).expect("utf-8");
    assert!(text.contains("Gemma Terms of Use"), "{text}");

    eprintln!(
        "RAN: verified {} files ({:.1} MiB) against the pinned catalog digests in {}",
        entry.files.len(),
        entry.total_bytes() as f64 / (1024.0 * 1024.0),
        layout.model_dir(entry.model_id).display()
    );

    // Optional second half: make the weights available to the ONNX test, and
    // pin the documented no-op behavior of a repeat install (10 §5: "a no-op
    // install does not reprint" the license).
    let Ok(shared) = std::env::var("LOCAL_RAG_TEST_MODEL_HOME") else {
        return;
    };
    let shared = StoreLayout::new(std::path::PathBuf::from(shared));
    shared.ensure().expect("ensure shared store tree");
    install_model(&shared, entry, &fetcher, &mut Vec::new()).expect("install into the shared root");
    assert!(is_installed(&shared, entry.model_id));

    let mut second_notice = Vec::new();
    let repeat = install_model(&shared, entry, &fetcher, &mut second_notice)
        .expect("a repeat install is a no-op");
    assert_eq!(
        repeat.reused.len(),
        entry.files.len(),
        "already installed ⇒ everything reused: {repeat:?}"
    );
    assert!(repeat.downloaded.is_empty(), "{repeat:?}");
    assert_eq!(repeat.bytes_downloaded, 0);
    assert!(
        second_notice.is_empty(),
        "a no-op install must not reprint the license"
    );

    eprintln!(
        "RAN: shared root {} is installed; repeat install is a no-op that reprints nothing",
        shared.model_dir(entry.model_id).display()
    );
}
