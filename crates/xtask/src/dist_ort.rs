//! `cargo xtask dist-ort` — fetches and verifies the ONNX Runtime shared
//! library each platform package bundles (spec 10 §1's runtime requirement,
//! ADR-0005's `ort`/`load-dynamic` choice, `crates/models/src/onnx.rs`'s
//! `bundled_ort_dylib_path`) — T17-03.
//!
//! Pinned URL + SHA-256 per platform, the same verify-before-trust shape
//! `crates/models::install` uses for model weights, but scoped to a build/
//! release tool rather than a user-facing runtime command: this never runs as
//! part of `cargo xtask ci` (it needs the network, like `bench`/
//! `memory-bench`) and it writes into a caller-chosen output directory, not a
//! `StoreLayout`. Archive extraction shells out to the system `tar` — a
//! dependency this module's own network fetch deliberately avoids in product
//! code (see `crates/models/src/fetch.rs`'s doc comment), but reasonable here:
//! this is a manually invoked, already-network-touching dev tool, not
//! something `cargo xtask ci` must run offline.
//!
//! `win32-x64`/`win32-arm64` are not in [`ORT_ASSETS`]: `cargo-zigbuild` does
//! not support Windows targets at all, so this machine has no reachable
//! Windows build to bundle a runtime into. Tracked as a `blocked` deviation in
//! `PROGRESS.md`, not a silent gap.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use local_rag_models::{AssetFetcher, FetchError, HttpFetcher};

/// One platform's pinned ONNX Runtime release archive.
pub struct OrtAsset {
    /// The npm platform-package key this bundles into (`platform.js`'s own
    /// naming convention, e.g. `darwin-arm64`).
    pub platform: &'static str,
    /// The upstream ONNX Runtime release tag this asset was fetched from.
    pub version: &'static str,
    /// The exact release-asset download URL.
    pub url: &'static str,
    /// The archive's pinned SHA-256 — the security-critical check here,
    /// `crates/models::install`'s digest-pin idiom applied to a build tool
    /// instead of a runtime one.
    pub archive_sha256: &'static str,
    /// Path, inside the extracted archive, to the *real* (non-symlink)
    /// shared-library file.
    ///
    /// ONNX Runtime's own release layout ships the fully versioned file name
    /// (`libonnxruntime.<version>.dylib`/`.so.<version>`) as a regular file
    /// and the unversioned name as a symlink to it — confirmed by inspecting
    /// all four archives below, where the unversioned entry is a symlink in
    /// three of the four and, in the fourth, a byte-identical duplicate
    /// (`tar -tvzf`, not documented upstream behavior either way). Always
    /// targeting the versioned member means bundling never depends on `tar`
    /// preserving a symlink, and the platform package ships exactly one file.
    pub archive_member: &'static str,
    /// The flat file name `bundled_ort_dylib_path` (`crates/models/src/
    /// onnx.rs`) looks for next to a product binary.
    pub dylib_name: &'static str,
}

/// The four platforms this machine can reach (spec 13 §1's tooling list; see
/// the module doc comment for why Windows is absent).
///
/// `darwin-x64` deliberately pins an older release than the other three:
/// ONNX Runtime dropped prebuilt Intel-Mac binaries as of v1.27.0, and
/// v1.20.0 is the newest tag that still ships `onnxruntime-osx-x86_64-*`.
/// `crates/models`'s own G11 evidence already validated v1.27.0 end to end on
/// the other three platforms, so this is the one platform where the pinned
/// version differs from what production inference was measured against.
pub const ORT_ASSETS: &[OrtAsset] = &[
    OrtAsset {
        platform: "darwin-arm64",
        version: "1.27.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-osx-arm64-1.27.0.tgz",
        archive_sha256: "545e81c58152353acb0d1e8bd6ce4b62f830c0961f5b3acfedc790ffd76e477a",
        archive_member: "onnxruntime-osx-arm64-1.27.0/lib/libonnxruntime.1.27.0.dylib",
        dylib_name: "libonnxruntime.dylib",
    },
    OrtAsset {
        platform: "darwin-x64",
        version: "1.20.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.20.0/onnxruntime-osx-x86_64-1.20.0.tgz",
        archive_sha256: "d28e603b47b74050f2c30a7069bf3fb371cfba7205d7771f22cabc7b02953757",
        archive_member: "onnxruntime-osx-x86_64-1.20.0/lib/libonnxruntime.1.20.0.dylib",
        dylib_name: "libonnxruntime.dylib",
    },
    OrtAsset {
        platform: "linux-x64",
        version: "1.27.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-linux-x64-1.27.0.tgz",
        archive_sha256: "547e40a48f1fe73e3f812d7c88a948612c23f896b91e4e2ee1e232d7b468246f",
        archive_member: "onnxruntime-linux-x64-1.27.0/lib/libonnxruntime.so.1.27.0",
        dylib_name: "libonnxruntime.so",
    },
    OrtAsset {
        platform: "linux-arm64",
        version: "1.27.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-linux-aarch64-1.27.0.tgz",
        archive_sha256: "3e4d83ac06924a32a07b6d7f91ce6f852876153fc0bbdf931bf517a140bfbe48",
        archive_member: "onnxruntime-linux-aarch64-1.27.0/lib/libonnxruntime.so.1.27.0",
        dylib_name: "libonnxruntime.so",
    },
];

/// Find a catalog entry by its platform key.
pub fn find(platform: &str) -> Option<&'static OrtAsset> {
    ORT_ASSETS.iter().find(|a| a.platform == platform)
}

/// Why `dist-ort` could not bundle a runtime.
#[derive(Debug)]
pub enum DistOrtError {
    /// `--platform` named a key not in [`ORT_ASSETS`].
    UnknownPlatform(String),
    /// Fetching the archive failed.
    Fetch(FetchError),
    /// The downloaded archive's SHA-256 does not match the pinned digest.
    ChecksumMismatch { expected: String, actual: String },
    /// A filesystem or `tar` operation failed.
    Io(String),
}

impl std::fmt::Display for DistOrtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DistOrtError::UnknownPlatform(p) => {
                let known: Vec<&str> = ORT_ASSETS.iter().map(|a| a.platform).collect();
                write!(f, "unknown platform {p:?}; known platforms: {known:?}")
            }
            DistOrtError::Fetch(e) => write!(f, "{e}"),
            DistOrtError::ChecksumMismatch { expected, actual } => write!(
                f,
                "archive checksum mismatch: expected {expected}, got {actual} \
                 (the pinned digest in dist_ort.rs no longer matches the release asset)"
            ),
            DistOrtError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for DistOrtError {
    fn from(e: io::Error) -> Self {
        DistOrtError::Io(e.to_string())
    }
}

/// Fetch, verify, and extract `asset`'s shared library into `out_dir/
/// <dylib_name>`.
///
/// Reuses the archive on disk (`out_dir/<archive file name>`) when it is
/// already present and its digest still matches the pin, so re-running this
/// for several platforms in a row does not re-download an unrelated asset's
/// bytes that already got the checksum right.
pub fn bundle(
    asset: &OrtAsset,
    out_dir: &Path,
    fetcher: &dyn AssetFetcher,
) -> Result<PathBuf, DistOrtError> {
    fs::create_dir_all(out_dir)?;

    let archive_name = asset.url.rsplit('/').next().unwrap_or(asset.url);
    let archive_path = out_dir.join(archive_name);

    let cached = fs::read(&archive_path).ok();
    let bytes = match cached.filter(|b| local_rag_core::hash::sha256_hex(b) == asset.archive_sha256)
    {
        Some(b) => b,
        None => {
            let mut buf = Vec::new();
            fetcher
                .fetch(asset.url, &mut buf)
                .map_err(DistOrtError::Fetch)?;
            let actual = local_rag_core::hash::sha256_hex(&buf);
            if actual != asset.archive_sha256 {
                return Err(DistOrtError::ChecksumMismatch {
                    expected: asset.archive_sha256.to_string(),
                    actual,
                });
            }
            fs::write(&archive_path, &buf)?;
            buf
        }
    };
    drop(bytes);

    let extract_dir = out_dir.join(".extract").join(asset.platform);
    if extract_dir.is_dir() {
        fs::remove_dir_all(&extract_dir)?;
    }
    fs::create_dir_all(&extract_dir)?;

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&extract_dir)
        .arg(asset.archive_member)
        .status()
        .map_err(|e| DistOrtError::Io(format!("spawning tar failed: {e}")))?;
    if !status.success() {
        return Err(DistOrtError::Io(format!(
            "tar -xzf {} -C {} {} exited with {status}",
            archive_path.display(),
            extract_dir.display(),
            asset.archive_member
        )));
    }

    let extracted = extract_dir.join(asset.archive_member);
    let dest = out_dir.join(asset.dylib_name);
    fs::copy(&extracted, &dest)?;
    fs::remove_dir_all(out_dir.join(".extract"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dest, perms)?;
    }

    Ok(dest)
}

/// `cargo xtask dist-ort --platform <key>|all --out <dir>`
///
/// `all` bundles every reachable platform into `<dir>/<platform>/`; a single
/// `--platform <key>` bundles directly into `<dir>`.
pub fn run() -> ExitCode {
    let mut platform: Option<String> = None;
    let mut out: Option<PathBuf> = None;

    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--platform" => platform = args.next(),
            "--out" => out = args.next().map(PathBuf::from),
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }

    let (Some(platform), Some(out)) = (platform, out) else {
        eprintln!("usage: cargo xtask dist-ort --platform <key>|all --out <dir>");
        eprintln!(
            "known platforms: {:?}",
            ORT_ASSETS.iter().map(|a| a.platform).collect::<Vec<_>>()
        );
        return ExitCode::from(2);
    };

    let fetcher = HttpFetcher::new();
    let targets: Vec<&OrtAsset> = if platform == "all" {
        ORT_ASSETS.iter().collect()
    } else {
        match find(&platform) {
            Some(a) => vec![a],
            None => {
                eprintln!("{}", DistOrtError::UnknownPlatform(platform));
                return ExitCode::from(2);
            }
        }
    };

    for asset in targets {
        let dest_dir = if platform == "all" {
            out.join(asset.platform)
        } else {
            out.clone()
        };
        print!("dist-ort: {} (ORT {}) ... ", asset.platform, asset.version);
        std::io::stdout().flush().ok();
        match bundle(asset, &dest_dir, &fetcher) {
            Ok(dest) => println!("OK -> {}", dest.display()),
            Err(e) => {
                println!("FAILED");
                eprintln!("dist-ort: {}: {e}", asset.platform);
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reachable_platform_is_cataloged_exactly_once() {
        let mut seen = std::collections::HashSet::new();
        for asset in ORT_ASSETS {
            assert!(
                seen.insert(asset.platform),
                "duplicate platform key: {}",
                asset.platform
            );
        }
        assert_eq!(
            seen,
            std::collections::HashSet::from([
                "darwin-arm64",
                "darwin-x64",
                "linux-x64",
                "linux-arm64",
            ]),
            "win32-x64/win32-arm64 are deliberately absent — see the module doc comment"
        );
    }

    #[test]
    fn every_pinned_digest_is_a_64_character_hex_string() {
        for asset in ORT_ASSETS {
            assert_eq!(
                asset.archive_sha256.len(),
                64,
                "{}: not a sha256 hex digest",
                asset.platform
            );
            assert!(
                asset.archive_sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "{}: not a sha256 hex digest",
                asset.platform
            );
        }
    }

    #[test]
    fn every_archive_member_path_is_rooted_inside_its_own_release_directory() {
        // Guards against a copy/paste slip pointing one platform at another
        // platform's extracted directory name.
        for asset in ORT_ASSETS {
            let archive_stem = asset
                .url
                .rsplit('/')
                .next()
                .unwrap()
                .trim_end_matches(".tgz");
            assert!(
                asset.archive_member.starts_with(archive_stem),
                "{}: archive_member {:?} is not rooted at {:?}",
                asset.platform,
                asset.archive_member,
                archive_stem
            );
        }
    }

    #[test]
    fn darwin_dylib_names_are_dylib_and_linux_are_so() {
        for asset in ORT_ASSETS {
            let expected = if asset.platform.starts_with("darwin") {
                "libonnxruntime.dylib"
            } else {
                "libonnxruntime.so"
            };
            assert_eq!(asset.dylib_name, expected, "{}", asset.platform);
        }
    }

    #[test]
    fn find_resolves_known_platforms_and_rejects_unknown_ones() {
        assert!(find("darwin-arm64").is_some());
        assert!(find("win32-x64").is_none());
    }

    #[test]
    fn an_unknown_platform_error_names_it_and_lists_the_known_ones() {
        let err = DistOrtError::UnknownPlatform("win32-x64".to_string());
        let message = err.to_string();
        assert!(message.contains("win32-x64"));
        assert!(message.contains("darwin-arm64"));
    }

    #[test]
    fn a_checksum_mismatch_names_both_digests() {
        let err = DistOrtError::ChecksumMismatch {
            expected: "a".repeat(64),
            actual: "b".repeat(64),
        };
        let message = err.to_string();
        assert!(message.contains(&"a".repeat(64)));
        assert!(message.contains(&"b".repeat(64)));
    }

    #[test]
    fn bundling_verifies_extracts_and_marks_the_dylib_executable() {
        let home = local_rag_test_support::home::TempHome::new().expect("temp home");
        let out_dir = home.join("bundle");

        let member_bytes = b"not a real onnxruntime, just fixture bytes".to_vec();
        let member_path = "onnxruntime-osx-arm64-1.27.0/lib/libonnxruntime.1.27.0.dylib";
        let fixture = FixtureFetcher::new(&home, member_path, &member_bytes);
        let asset = fixture.asset();

        let dest = bundle(&asset, &out_dir, &fixture).expect("bundle");
        assert_eq!(dest, out_dir.join("libonnxruntime.dylib"));
        assert_eq!(fs::read(&dest).expect("read"), member_bytes);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dest).expect("meta").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "the bundled dylib must be executable");
        }

        // The extraction scratch directory does not leak into the bundle.
        assert!(!out_dir.join(".extract").exists());
    }

    #[test]
    fn a_wrong_digest_is_rejected_before_anything_is_extracted() {
        let home = local_rag_test_support::home::TempHome::new().expect("temp home");
        let out_dir = home.join("bundle");

        let member_path = "onnxruntime-osx-arm64-1.27.0/lib/libonnxruntime.1.27.0.dylib";
        let fixture = FixtureFetcher::new(&home, member_path, b"genuine bytes");
        let mut asset = fixture.asset();
        // Pin a digest that does not match what `fixture` actually serves.
        asset.archive_sha256 = "0".repeat(64).leak();

        let err = bundle(&asset, &out_dir, &fixture).expect_err("digest must not match");
        assert!(
            matches!(err, DistOrtError::ChecksumMismatch { .. }),
            "{err}"
        );
        assert!(
            !out_dir.join(asset.dylib_name).exists(),
            "nothing is bundled on a checksum failure"
        );
    }

    #[test]
    fn a_cached_archive_with_a_matching_digest_is_reused_without_a_second_fetch() {
        let home = local_rag_test_support::home::TempHome::new().expect("temp home");
        let out_dir = home.join("bundle");

        let member_bytes = b"not a real onnxruntime, just fixture bytes".to_vec();
        let member_path = "onnxruntime-osx-arm64-1.27.0/lib/libonnxruntime.1.27.0.dylib";
        let fixture = FixtureFetcher::new(&home, member_path, &member_bytes);
        let asset = fixture.asset();
        bundle(&asset, &out_dir, &fixture).expect("first bundle");

        let poisoned = PoisonedFetcher;
        let dest = bundle(&asset, &out_dir, &poisoned).expect("second bundle reuses the cache");
        assert_eq!(fs::read(&dest).expect("read"), member_bytes);
    }

    /// Builds a real `.tgz` (by shelling out to the same `tar` `bundle` itself
    /// extracts with) containing exactly one file at `member_path`, and serves
    /// those archive bytes in place of the network — so `bundle`'s real
    /// tar-extraction and digest-check logic run for real without touching
    /// github.com. [`FixtureFetcher::asset`] returns a throwaway [`OrtAsset`]
    /// whose `archive_sha256` is the *fixture's own* digest, not any of
    /// [`ORT_ASSETS`]'s pinned ones — those name real upstream bytes this
    /// fixture never reproduces.
    struct FixtureFetcher {
        archive_bytes: Vec<u8>,
        archive_sha256: String,
        member_path: &'static str,
    }

    impl FixtureFetcher {
        fn new(
            home: &local_rag_test_support::home::TempHome,
            member_path: &'static str,
            contents: &[u8],
        ) -> Self {
            let staging = home.join("fixture-staging");
            let member_file = staging.join(member_path);
            fs::create_dir_all(member_file.parent().expect("member has a parent"))
                .expect("mkdir staging");
            fs::write(&member_file, contents).expect("write fixture member");

            let archive_path = home.join("fixture-source.tgz");
            let status = Command::new("tar")
                .arg("-czf")
                .arg(&archive_path)
                .arg("-C")
                .arg(&staging)
                .arg(member_path)
                .status()
                .expect("spawn tar -czf to build the test fixture");
            assert!(
                status.success(),
                "tar -czf failed while building the fixture"
            );

            let archive_bytes = fs::read(&archive_path).expect("read built fixture archive");
            let archive_sha256 = local_rag_core::hash::sha256_hex(&archive_bytes);
            FixtureFetcher {
                archive_bytes,
                archive_sha256,
                member_path,
            }
        }

        fn asset(&self) -> OrtAsset {
            OrtAsset {
                platform: "darwin-arm64",
                version: "0.0.0-fixture",
                url: "https://example.invalid/fixture-source.tgz",
                archive_sha256: self.archive_sha256.clone().leak(),
                archive_member: self.member_path,
                dylib_name: "libonnxruntime.dylib",
            }
        }
    }

    impl AssetFetcher for FixtureFetcher {
        fn fetch(
            &self,
            _url: &str,
            sink: &mut dyn Write,
        ) -> Result<u64, local_rag_models::FetchError> {
            sink.write_all(&self.archive_bytes)?;
            Ok(self.archive_bytes.len() as u64)
        }
    }

    struct PoisonedFetcher;
    impl AssetFetcher for PoisonedFetcher {
        fn fetch(
            &self,
            url: &str,
            _sink: &mut dyn Write,
        ) -> Result<u64, local_rag_models::FetchError> {
            panic!("must not fetch when a valid cached archive already exists: {url}");
        }
    }
}
