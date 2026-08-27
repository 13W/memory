//! `cargo xtask dist-ort` — fetches and verifies the ONNX Runtime shared
//! library for one platform into a caller-chosen directory.
//!
//! # What moved out of this file in T22-15
//!
//! The pinned catalog (`OrtAsset`/`ORT_ASSETS`) now lives in
//! [`local_rag_models::ort_catalog`], because ADR-0013 made the runtime an
//! artifact of first run: the *product* needs the pins, and `xtask` already
//! depends on that crate, so the table could only move in that direction. This
//! file reads it from there and defines none of its own.
//!
//! Extraction moved with it. This module used to shell out to the system `tar`,
//! with a doc comment arguing that was reasonable for a manually invoked dev
//! tool even though product code may not do it. That argument is now moot:
//! `local_rag_models::archive` exists because the installer needed it, it
//! handles the `.zip` the newly pinned `win32-x64` asset ships in (which `tar
//! -xzf` would not have), and running the same reader here means the release
//! tool and the runtime cannot disagree about what "the member" is.
//!
//! What has not changed: this never runs as part of `cargo xtask ci` (it needs
//! the network, like `bench`/`memory-bench`), and it writes into a
//! caller-chosen output directory, not a `StoreLayout`.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use local_rag_models::archive::{ArchiveError, Limits as ArchiveLimits, extract_member};
use local_rag_models::ort_catalog::{ORT_ASSETS, OrtAsset, find};
use local_rag_models::{AssetFetcher, FetchError, HttpFetcher};

/// Why `dist-ort` could not bundle a runtime.
#[derive(Debug)]
pub enum DistOrtError {
    /// `--platform` named a key not in [`ORT_ASSETS`].
    UnknownPlatform(String),
    /// Fetching the archive failed.
    Fetch(FetchError),
    /// The downloaded archive's SHA-256 does not match the pinned digest.
    ChecksumMismatch { expected: String, actual: String },
    /// Reading the member out of the archive failed.
    Archive(ArchiveError),
    /// A filesystem operation failed.
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
                 (the pinned digest in local_rag_models::ort_catalog no longer matches \
                 the release asset)"
            ),
            DistOrtError::Archive(e) => write!(f, "{e}"),
            DistOrtError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<ArchiveError> for DistOrtError {
    fn from(e: ArchiveError) -> Self {
        DistOrtError::Archive(e)
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

    let dest = out_dir.join(asset.dylib_name);
    let part = dest.with_extension("part");
    {
        let mut sink = File::create(&part)?;
        extract_member(
            &archive_path,
            asset.archive_format,
            asset.archive_member,
            &mut sink,
            &ArchiveLimits::default(),
        )?;
    }
    fs::rename(&part, &dest)?;

    // 0755 here, unlike the installer's 0600: this writes a directory a release
    // process hands on, not a per-user store the spec locks down (12 §6).
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
    use local_rag_models::archive::ArchiveFormat;

    // The catalog's own shape — one entry per platform, digest form, member
    // paths, library names — is asserted where the catalog now lives
    // (`local_rag_models::ort_catalog`, T22-15). What is left here is this
    // tool's own behaviour: fetching, verifying and extracting one asset.

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

    /// Builds a real `.tgz` in memory containing exactly one file at
    /// `member_path`, and serves those archive bytes in place of the network —
    /// so `bundle`'s real extraction and digest-check logic run without
    /// touching github.com. [`FixtureFetcher::asset`] returns a throwaway
    /// [`OrtAsset`] whose digests are the *fixture's own*, not any of
    /// [`ORT_ASSETS`]'s pinned ones — those name real upstream bytes this
    /// fixture never reproduces.
    ///
    /// It used to shell out to `tar -czf`, on the argument that it should build
    /// its fixture with the same tool `bundle` extracted with. T22-15 removed
    /// that tool from `bundle`, so the argument went with it; building the
    /// bytes here also means this test no longer depends on which `tar` the
    /// host has.
    struct FixtureFetcher {
        archive_bytes: Vec<u8>,
        archive_sha256: String,
        member_path: &'static str,
        dylib_sha256: String,
        dylib_size: u64,
    }

    impl FixtureFetcher {
        fn new(
            _home: &local_rag_test_support::home::TempHome,
            member_path: &'static str,
            contents: &[u8],
        ) -> Self {
            let mut header = [0u8; 512];
            header[..member_path.len()].copy_from_slice(member_path.as_bytes());
            header[100..107].copy_from_slice(b"0000644");
            header[108..115].copy_from_slice(b"0000000");
            header[116..123].copy_from_slice(b"0000000");
            let size = format!("{:011o} ", contents.len());
            header[124..124 + size.len()].copy_from_slice(size.as_bytes());
            header[136..147].copy_from_slice(b"00000000000");
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].copy_from_slice(b"        ");
            let sum: u32 = header.iter().map(|&b| b as u32).sum();
            let chk = format!("{sum:06o}\0 ");
            header[148..148 + chk.len()].copy_from_slice(chk.as_bytes());

            let mut tar = Vec::new();
            tar.extend_from_slice(&header);
            tar.extend_from_slice(contents);
            tar.extend(std::iter::repeat_n(0u8, (512 - contents.len() % 512) % 512));
            tar.extend(std::iter::repeat_n(0u8, 1024));

            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&tar).expect("gzip the fixture");
            let archive_bytes = encoder.finish().expect("finish gzip");
            let archive_sha256 = local_rag_core::hash::sha256_hex(&archive_bytes);
            FixtureFetcher {
                archive_bytes,
                archive_sha256,
                member_path,
                dylib_sha256: local_rag_core::hash::sha256_hex(contents),
                dylib_size: contents.len() as u64,
            }
        }

        fn asset(&self) -> OrtAsset {
            OrtAsset {
                platform: "darwin-arm64",
                version: "0.0.0-fixture",
                url: "https://example.invalid/fixture-source.tgz",
                archive_format: ArchiveFormat::TarGz,
                archive_size: self.archive_bytes.len() as u64,
                archive_sha256: self.archive_sha256.clone().leak(),
                archive_member: self.member_path,
                dylib_name: "libonnxruntime.dylib",
                dylib_size: self.dylib_size,
                dylib_sha256: self.dylib_sha256.clone().leak(),
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
