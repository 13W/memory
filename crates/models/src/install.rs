//! The model asset installer (spec 10 §5 `[FIXED policy]`) — T11-06.
//!
//! > `local-rag init --download-models`: checksum-verified manifest, atomic
//! > download (`.part` → fsync → rename → `.ok` marker), offline operation
//! > afterwards. `models/<model_id>/manifest.json` records source, size, sha256,
//! > license.
//!
//! # The `.ok` marker is the whole contract
//!
//! `local_rag_embed::require_model_assets` (T11-03) treats a model directory as
//! usable **only** when `.ok` exists. This module is the only writer of that
//! marker, and it writes it last — after every file has been verified and after
//! `manifest.json` is durable. Everything before that point is, by construction,
//! indistinguishable from "not installed": a half-downloaded `.part`, a complete
//! set of files with no manifest, a manifest with no marker. That is what makes
//! an interrupted install safe rather than merely recoverable.
//!
//! # Resumable without a journal
//!
//! There is no progress file. Each run re-derives what is missing by hashing
//! what is already on disk against the catalog's pinned digests, so an interrupt
//! at any point is healed by running again — the same "recompute, don't
//! journal" model `local_rag_store::retention`'s sweep and T11-04's backfill
//! use. A `.part` left behind is simply overwritten; it is never trusted,
//! because nothing recorded how far it got.
//!
//! # Durability
//!
//! Each file is written to `<name>.part`, `sync_all`'d, verified against its
//! pinned size and sha256, renamed into place, and then the **directory** is
//! fsync'd so the rename itself is durable. A rename that survives a crash but
//! whose directory entry does not would leave a file that appears absent on the
//! next boot — the exact failure the marker ordering is meant to exclude.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use local_rag_core::paths::{PathError, StoreLayout, perms};

use crate::archive::{self, ArchiveError, Limits as ArchiveLimits};
use crate::catalog::{AssetFile, ModelCatalogEntry};
use crate::fetch::{AssetFetcher, FetchError};
use crate::manifest::{ManifestFile, ModelManifest, OrtManifest};
use crate::ort_catalog::OrtAsset;

/// The marker file that makes a model directory usable (spec 10 §5).
pub const OK_MARKER: &str = ".ok";

/// The manifest file recording source/size/sha256/license (spec 10 §5).
pub const MANIFEST_FILE: &str = "manifest.json";

/// Suffix of an in-flight download.
pub const PART_SUFFIX: &str = ".part";

/// What one install run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallReport {
    /// The model that was installed.
    pub model_id: String,
    /// Files downloaded by this run.
    pub downloaded: Vec<String>,
    /// Files already present with a matching digest, and therefore left alone.
    pub reused: Vec<String>,
    /// Total bytes downloaded by this run.
    pub bytes_downloaded: u64,
    /// Whether this run was the one that wrote the `.ok` marker.
    pub marked_ready: bool,
}

impl InstallReport {
    /// Whether the run had nothing to do.
    pub fn is_noop(&self) -> bool {
        self.downloaded.is_empty() && !self.marked_ready
    }
}

/// Why an install failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum InstallError {
    /// The model id is not in this build's catalog.
    UnknownModel {
        /// The id that was requested.
        model_id: String,
    },
    /// Fetching bytes failed.
    Fetch(FetchError),
    /// A downloaded file's SHA-256 does not match the catalog's pinned digest.
    ///
    /// The `.part` is left in place for inspection but never renamed, so the
    /// model stays "not installed" and a retry simply overwrites it.
    ChecksumMismatch {
        /// The file that failed.
        file: String,
        /// The digest the catalog pins.
        expected: String,
        /// The digest the received bytes actually hash to.
        actual: String,
    },
    /// A downloaded file's length does not match the catalog's pinned size.
    SizeMismatch {
        /// The file that failed.
        file: String,
        /// The size the catalog pins.
        expected: u64,
        /// The size actually received.
        actual: u64,
    },
    /// Taking the library out of a downloaded archive failed.
    Archive(ArchiveError),
    /// A filesystem operation failed.
    Io(io::Error),
    /// Creating the model directory with the required permissions failed.
    Path(PathError),
    /// The named crash point fired (test builds only). Everything already
    /// renamed into place stays; the next run resumes from it.
    #[cfg(feature = "failpoints")]
    Interrupted,
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::UnknownModel { model_id } => {
                write!(f, "unknown model {model_id}")
            }
            InstallError::Fetch(e) => write!(f, "download failed: {e}"),
            InstallError::ChecksumMismatch {
                file,
                expected,
                actual,
            } => write!(
                f,
                "checksum mismatch for {file}: expected {expected}, got {actual}"
            ),
            InstallError::SizeMismatch {
                file,
                expected,
                actual,
            } => write!(
                f,
                "size mismatch for {file}: expected {expected} bytes, got {actual}"
            ),
            InstallError::Archive(e) => write!(f, "extracting the runtime failed: {e}"),
            InstallError::Io(e) => write!(f, "filesystem error during install: {e}"),
            InstallError::Path(e) => write!(f, "could not prepare the model directory: {e}"),
            #[cfg(feature = "failpoints")]
            InstallError::Interrupted => write!(f, "install interrupted between files"),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InstallError::Fetch(e) => Some(e),
            InstallError::Archive(e) => Some(e),
            InstallError::Io(e) => Some(e),
            InstallError::Path(e) => Some(e),
            _ => None,
        }
    }
}

impl From<FetchError> for InstallError {
    fn from(e: FetchError) -> Self {
        InstallError::Fetch(e)
    }
}

impl From<io::Error> for InstallError {
    fn from(e: io::Error) -> Self {
        InstallError::Io(e)
    }
}

impl From<PathError> for InstallError {
    fn from(e: PathError) -> Self {
        InstallError::Path(e)
    }
}

/// Whether `model_id` is installed and usable.
///
/// Deliberately thin, and deliberately the *same* rule the consumer applies:
/// `local_rag_embed::require_model_assets` is the authority on "usable", and
/// this only mirrors its marker check for callers that want to ask before
/// paying for a provider.
pub fn is_installed(layout: &StoreLayout, model_id: &str) -> bool {
    layout.model_dir(model_id).join(OK_MARKER).is_file()
}

/// Print the license and source a download implies, before any bytes move.
///
/// ADR-0004 requires the installer to "surface the Gemma Terms (source URL and
/// license string) **before download**". It writes to a caller-supplied sink and
/// never prompts: `local-rag init` has to stay scriptable, and no spec text asks
/// for interactivity.
pub fn write_license_notice(entry: &ModelCatalogEntry, out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "Model:   {} ({} dimensions)",
        entry.model_id, entry.dimensions
    )?;
    writeln!(out, "Source:  {}", entry.source)?;
    writeln!(out, "Revision: {}", entry.revision)?;
    writeln!(out, "License: {} — {}", entry.license, entry.license_url)?;
    writeln!(
        out,
        "Download size: {:.1} MiB across {} file(s).",
        entry.total_bytes() as f64 / (1024.0 * 1024.0),
        entry.files.len()
    )?;
    writeln!(
        out,
        "Downloading these weights means accepting the license above; \
         local-rag redistributes no weights."
    )
}

/// Install (or complete a partial install of) `entry` under
/// `<store>/models/<model_id>/`.
///
/// Idempotent and resumable: files whose digest already matches are reused,
/// missing ones are fetched, and the `.ok` marker is written last. Running it
/// again after a completed install is a no-op.
///
/// `notice` receives the license notice before the first fetch; pass
/// `&mut std::io::sink()` when a caller has already surfaced it.
pub fn install_model(
    layout: &StoreLayout,
    entry: &ModelCatalogEntry,
    fetcher: &dyn AssetFetcher,
    notice: &mut dyn Write,
) -> Result<InstallReport, InstallError> {
    let dir = layout.model_dir(entry.model_id);
    let mut report = InstallReport {
        model_id: entry.model_id.to_string(),
        ..InstallReport::default()
    };

    // An already-marked directory is done. Checked before the notice: a no-op
    // install should not re-print a license the user already accepted.
    if is_installed(layout, entry.model_id) {
        report.reused = entry
            .files
            .iter()
            .map(|f| f.relative_path.to_string())
            .collect();
        return Ok(report);
    }

    write_license_notice(entry, notice)?;
    perms::ensure_dir(&dir)?;

    for file in entry.files {
        let target = dir.join(file.relative_path);
        if let Some(parent) = target.parent()
            && parent != dir
        {
            perms::ensure_dir(parent)?;
        }

        // Reuse only what verifies: a file of the right name but wrong bytes is
        // as good as absent (spec 10 §5's "checksum-verified").
        if target.is_file() && file_matches(&target, file)? {
            report.reused.push(file.relative_path.to_string());
            continue;
        }

        let bytes = download_verified(fetcher, entry, file, &target)?;
        report.downloaded.push(file.relative_path.to_string());
        report.bytes_downloaded += bytes;

        // Crash point *after* a file is durably in place — the resume test kills
        // here and asserts the next run keeps it and continues.
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!(
            "models.install.between_files",
            Err(InstallError::Interrupted)
        );
    }

    // The manifest records what is now on disk (spec 10 §5's source/size/
    // sha256/license), written atomically like any other asset...
    let manifest = ModelManifest {
        model_id: entry.model_id.to_string(),
        source: entry.source.to_string(),
        revision: entry.revision.to_string(),
        license: entry.license.to_string(),
        license_url: entry.license_url.to_string(),
        dimensions: entry.dimensions,
        files: entry
            .files
            .iter()
            .map(|f| ManifestFile {
                path: f.relative_path.to_string(),
                size: f.size,
                sha256: f.sha256.to_string(),
            })
            .collect(),
    };
    atomic_write(&dir.join(MANIFEST_FILE), manifest.to_json().as_bytes())?;

    // ... and only then the marker that makes the directory usable.
    atomic_write(&dir.join(OK_MARKER), b"")?;
    report.marked_ready = true;
    Ok(report)
}

// ---------------------------------------------------------------------------
// ONNX Runtime (T22-15)
// ---------------------------------------------------------------------------

/// Where a runtime version lives: `<store>/models/onnxruntime/<version>/`.
///
/// Under `models/` because spec 10 §5 `[FIXED, ADR-0013]` says "installed at
/// first run **beside the weights**, by the same verified path"; keyed by
/// version so a pin change installs alongside rather than over the library a
/// running process may have open — replacing a `dlopen`ed file in place is how
/// a live daemon gets a segfault instead of an upgrade.
///
/// The cost of that choice, stated rather than left to be discovered: nothing
/// here removes the previous version's directory, so a store accumulates ~30 MB
/// per pin change. Reporting and reclaiming that is `doctor`'s job (T22-16),
/// which is also the card that gives this installer a caller.
pub fn ort_dir(layout: &StoreLayout, asset: &OrtAsset) -> PathBuf {
    layout.models_dir().join("onnxruntime").join(asset.version)
}

/// The installed library's own path.
pub fn ort_dylib_path(layout: &StoreLayout, asset: &OrtAsset) -> PathBuf {
    ort_dir(layout, asset).join(asset.dylib_name)
}

/// Whether this platform's pinned runtime is installed and usable.
///
/// The same rule the weights use: the `.ok` marker, and nothing else. Anything
/// short of it — a half-extracted `.part`, a library with no manifest — is
/// indistinguishable from "not installed" by construction.
pub fn ort_is_installed(layout: &StoreLayout, asset: &OrtAsset) -> bool {
    ort_dir(layout, asset).join(OK_MARKER).is_file()
}

/// What one runtime install run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrtInstallReport {
    /// The platform key installed for.
    pub platform: String,
    /// The upstream version installed.
    pub version: String,
    /// The installed library.
    pub path: PathBuf,
    /// Bytes pulled over the network by this run (0 when nothing was fetched).
    pub bytes_downloaded: u64,
    /// Whether this run was the one that wrote the `.ok` marker.
    pub marked_ready: bool,
}

/// Install (or complete a partial install of) `asset`'s shared library.
///
/// # Two digests, and neither is redundant
///
/// The weights installer verifies each file it downloads, because each file is
/// what it installs. Here the download and the installed file are different
/// bytes: an archive comes down the wire and one member comes out of it. So
/// `archive_sha256` is checked before anything is unpacked — that is the
/// wire/tamper boundary — and `dylib_sha256` is checked before the library is
/// renamed into place, which is what catches this project's own extractor
/// putting the wrong bytes somewhere. Verifying only the input would trust code
/// that was written for this task.
///
/// # The archive is scaffolding, and it is cleaned up
///
/// It is kept next to the `.part` until the marker is written, so an interrupt
/// mid-extract resumes without re-downloading 77 MB, and removed once the
/// install is complete so a finished store does not hoard it.
pub fn install_ort(
    layout: &StoreLayout,
    asset: &OrtAsset,
    fetcher: &dyn AssetFetcher,
) -> Result<OrtInstallReport, InstallError> {
    let dir = ort_dir(layout, asset);
    let dylib = dir.join(asset.dylib_name);
    let mut report = OrtInstallReport {
        platform: asset.platform.to_string(),
        version: asset.version.to_string(),
        path: dylib.clone(),
        bytes_downloaded: 0,
        marked_ready: false,
    };

    if ort_is_installed(layout, asset) {
        return Ok(report);
    }

    // Two levels, created one at a time. `perms::ensure_dir` is not
    // `create_dir_all`: on Unix it builds a single directory so each one gets
    // 0700 and an ownership check (`crates/core/src/paths/perms.rs`), and
    // `install_model` already creates nested parents the same way. A
    // `create_dir_all` here would make `models/onnxruntime/` inherit the
    // process umask instead — spec 02 §2.1 / 12 §6 `[FIXED]`.
    perms::ensure_dir(dir.parent().expect("ort_dir always has a parent"))?;
    perms::ensure_dir(&dir)?;

    let archive_name = asset
        .url
        .rsplit('/')
        .next()
        .unwrap_or("onnxruntime-archive");
    let archive_path = dir.join(archive_name);

    // Reuse only what verifies — the same rule the weights follow. A leftover
    // archive of the right name and wrong bytes is as good as absent.
    let archive_ready = archive_path.is_file()
        && fs::metadata(&archive_path)?.len() == asset.archive_size
        && sha256_file(&archive_path)? == asset.archive_sha256;

    if !archive_ready {
        let part = part_path(&archive_path);
        let mut sink = HashingWriter::create(&part)?;
        fetcher.fetch(asset.url, &mut sink)?;
        let (written, digest) = sink.finish()?;
        if written != asset.archive_size {
            return Err(InstallError::SizeMismatch {
                file: archive_name.to_string(),
                expected: asset.archive_size,
                actual: written,
            });
        }
        if digest != asset.archive_sha256 {
            return Err(InstallError::ChecksumMismatch {
                file: archive_name.to_string(),
                expected: asset.archive_sha256.to_string(),
                actual: digest,
            });
        }
        fs::rename(&part, &archive_path)?;
        sync_dir(&dir)?;
        report.bytes_downloaded = written;
    }

    let part = part_path(&dylib);
    let mut sink = HashingWriter::create(&part)?;
    archive::extract_member(
        &archive_path,
        asset.archive_format,
        asset.archive_member,
        &mut sink,
        &ArchiveLimits::default(),
    )
    .map_err(InstallError::Archive)?;
    let (written, digest) = sink.finish()?;
    if written != asset.dylib_size {
        return Err(InstallError::SizeMismatch {
            file: asset.dylib_name.to_string(),
            expected: asset.dylib_size,
            actual: written,
        });
    }
    if digest != asset.dylib_sha256 {
        return Err(InstallError::ChecksumMismatch {
            file: asset.dylib_name.to_string(),
            expected: asset.dylib_sha256.to_string(),
            actual: digest,
        });
    }
    fs::rename(&part, &dylib)?;
    sync_dir(&dir)?;
    set_file_mode(&dylib)?;

    // Crash point *between* a durable library and the marker that makes it
    // usable — the instant a `kill -9` is most awkward here, and the one the
    // ordering exists to make safe. Without it nothing observes the ordering:
    // a mutation that wrote the marker early left every test green.
    #[cfg(feature = "failpoints")]
    local_rag_test_support::fail_point!(
        "models.install.ort_before_marker",
        Err(InstallError::Interrupted)
    );

    let manifest = OrtManifest {
        platform: asset.platform.to_string(),
        version: asset.version.to_string(),
        source: asset.url.to_string(),
        archive_sha256: asset.archive_sha256.to_string(),
        file: asset.dylib_name.to_string(),
        size: asset.dylib_size,
        sha256: asset.dylib_sha256.to_string(),
    };
    atomic_write(&dir.join(MANIFEST_FILE), manifest.to_json().as_bytes())?;
    atomic_write(&dir.join(OK_MARKER), b"")?;
    report.marked_ready = true;

    // Only now: before the marker it is the thing that makes a retry cheap.
    let _ = fs::remove_file(&archive_path);
    Ok(report)
}

/// Fetch one file into `<target>.part`, verify it, and rename it into place.
fn download_verified(
    fetcher: &dyn AssetFetcher,
    entry: &ModelCatalogEntry,
    file: &AssetFile,
    target: &Path,
) -> Result<u64, InstallError> {
    let part = part_path(target);
    // A leftover `.part` is never trusted — nothing recorded how far it got.
    let mut sink = HashingWriter::create(&part)?;
    fetcher.fetch(&entry.url_for(file), &mut sink)?;
    let (written, digest) = sink.finish()?;

    if written != file.size {
        return Err(InstallError::SizeMismatch {
            file: file.relative_path.to_string(),
            expected: file.size,
            actual: written,
        });
    }
    if digest != file.sha256 {
        return Err(InstallError::ChecksumMismatch {
            file: file.relative_path.to_string(),
            expected: file.sha256.to_string(),
            actual: digest,
        });
    }

    fs::rename(&part, target)?;
    sync_dir(target.parent().unwrap_or(Path::new(".")))?;
    set_file_mode(target)?;
    Ok(written)
}

/// `<path>.part`.
fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(PART_SUFFIX);
    target.with_file_name(name)
}

/// Whether an on-disk file matches the catalog's pinned size and digest.
fn file_matches(path: &Path, file: &AssetFile) -> Result<bool, InstallError> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != file.size {
        return Ok(false);
    }
    Ok(sha256_file(path)? == file.sha256)
}

/// Stream a file through SHA-256 without holding it in memory.
pub(crate) fn sha256_file(path: &Path) -> io::Result<String> {
    let mut reader = File::open(path)?;
    let mut hasher = HashingWriter::in_memory();
    io::copy(&mut reader, &mut hasher)?;
    Ok(hasher.digest())
}

/// Write `bytes` to `path` atomically (temp + rename + directory fsync).
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), InstallError> {
    let tmp = part_path(path);
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_dir(path.parent().unwrap_or(Path::new(".")))?;
    set_file_mode(path)?;
    Ok(())
}

/// fsync a directory so a rename inside it is durable.
///
/// Skipped where the platform has no such notion (Windows cannot open a
/// directory as a file); the rename itself is still atomic there.
fn sync_dir(dir: &Path) -> io::Result<()> {
    match File::open(dir) {
        Ok(handle) => handle.sync_all(),
        // A directory that cannot be opened for sync is not a reason to fail an
        // otherwise complete install.
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(e) => Err(e),
    }
}

/// Tighten an installed file to 0600 (spec 02 §2.1 / 12 §6 `[FIXED]`).
#[cfg(unix)]
fn set_file_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// A writer that hashes everything passing through it.
///
/// Hashing while writing is what keeps a 295 MB asset off the heap: the
/// installer never reads a downloaded file back to verify it.
struct HashingWriter {
    file: Option<File>,
    hasher: local_rag_core::hash::Sha256,
    written: u64,
}

impl HashingWriter {
    fn create(path: &Path) -> io::Result<Self> {
        Ok(HashingWriter {
            file: Some(File::create(path)?),
            hasher: local_rag_core::hash::Sha256::new(),
            written: 0,
        })
    }

    fn in_memory() -> Self {
        HashingWriter {
            file: None,
            hasher: local_rag_core::hash::Sha256::new(),
            written: 0,
        }
    }

    fn digest(self) -> String {
        self.hasher.finish_hex()
    }

    /// Flush and fsync, returning the byte count and digest.
    fn finish(mut self) -> io::Result<(u64, String)> {
        if let Some(file) = self.file.take() {
            file.sync_all()?;
        }
        let written = self.written;
        Ok((written, self.hasher.finish_hex()))
    }
}

impl Write for HashingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(buf)?;
        }
        self.hasher.update(buf);
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_paths_append_rather_than_replace_the_extension() {
        // `Path::with_extension` would turn `model.onnx` into `model.part`,
        // colliding with a sibling asset; the suffix must be appended.
        assert_eq!(
            part_path(Path::new("/m/model_quantized.onnx")),
            Path::new("/m/model_quantized.onnx.part")
        );
        assert_eq!(
            part_path(Path::new("/m/model_quantized.onnx_data")),
            Path::new("/m/model_quantized.onnx_data.part")
        );
        assert_eq!(part_path(Path::new("/m/.ok")), Path::new("/m/.ok.part"));
    }

    #[test]
    fn a_hashing_writer_agrees_with_the_one_shot_hash() {
        let payload = b"weights, but small";
        let mut w = HashingWriter::in_memory();
        w.write_all(payload).expect("write");
        assert_eq!(w.digest(), local_rag_core::hash::sha256_hex(payload));
    }

    #[test]
    fn a_hashing_writer_is_chunk_size_independent() {
        let payload: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut single = HashingWriter::in_memory();
        single.write_all(&payload).expect("write");
        let mut chunked = HashingWriter::in_memory();
        for chunk in payload.chunks(97) {
            chunked.write_all(chunk).expect("write");
        }
        assert_eq!(single.digest(), chunked.digest());
    }

    #[test]
    fn a_completed_report_is_not_a_noop_but_a_repeat_is() {
        let done = InstallReport {
            model_id: "m".to_string(),
            downloaded: vec!["a".to_string()],
            marked_ready: true,
            ..InstallReport::default()
        };
        assert!(!done.is_noop());
        let repeat = InstallReport {
            model_id: "m".to_string(),
            reused: vec!["a".to_string()],
            ..InstallReport::default()
        };
        assert!(repeat.is_noop());
    }
}
