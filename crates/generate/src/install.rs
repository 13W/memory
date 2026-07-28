//! The generator model asset installer (spec 10 §5 `[FIXED policy]`) —
//! T14-07/ADR-0006, mirroring `local_rag_models::install` (see this crate's
//! own module doc and Cargo.toml comment for why the pattern is duplicated
//! rather than shared).
//!
//! > `local-rag init --download-models`: checksum-verified manifest, atomic
//! > download (`.part` → fsync → rename → `.ok` marker), offline operation
//! > afterwards. `models/<model_id>/manifest.json` records source, size,
//! > sha256, license.
//!
//! The `.ok` marker is the whole contract, written last, after every file is
//! verified and the manifest is durable — everything before that point is,
//! by construction, indistinguishable from "not installed". Resumable
//! without a journal: each run re-derives what is missing by hashing what is
//! already on disk against the catalog's pinned digests.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use local_rag_core::paths::{PathError, StoreLayout, perms};

use crate::catalog::{AssetFile, GeneratorCatalogEntry};
use crate::fetch::{AssetFetcher, FetchError};
use crate::manifest::{GeneratorManifest, GeneratorManifestFile};

/// The marker file that makes a model directory usable (spec 10 §5).
pub const OK_MARKER: &str = ".ok";

/// The manifest file recording source/size/sha256/license (spec 10 §5).
pub const MANIFEST_FILE: &str = "manifest.json";

/// Suffix of an in-flight download.
pub const PART_SUFFIX: &str = ".part";

/// What one install run did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallReport {
    pub model_id: String,
    pub downloaded: Vec<String>,
    pub reused: Vec<String>,
    pub bytes_downloaded: u64,
    pub marked_ready: bool,
}

impl InstallReport {
    pub fn is_noop(&self) -> bool {
        self.downloaded.is_empty() && !self.marked_ready
    }
}

/// Why an install failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum InstallError {
    UnknownModel {
        model_id: String,
    },
    Fetch(FetchError),
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },
    SizeMismatch {
        file: String,
        expected: u64,
        actual: u64,
    },
    Io(io::Error),
    Path(PathError),
    /// The named crash point fired (test builds only). Everything already
    /// renamed into place stays; the next run resumes from it.
    #[cfg(feature = "failpoints")]
    Interrupted,
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallError::UnknownModel { model_id } => write!(f, "unknown model {model_id}"),
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
pub fn is_installed(layout: &StoreLayout, model_id: &str) -> bool {
    layout.model_dir(model_id).join(OK_MARKER).is_file()
}

/// Print the license and source a download implies, before any bytes move
/// (mirrors ADR-0004's own "surface the license before download" obligation,
/// applied here per ADR-0006).
pub fn write_license_notice(entry: &GeneratorCatalogEntry, out: &mut dyn Write) -> io::Result<()> {
    writeln!(
        out,
        "Model:   {} ({} ctx)",
        entry.model_id, entry.context_length
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
/// `<store>/models/<model_id>/`. Idempotent and resumable.
pub fn install_model(
    layout: &StoreLayout,
    entry: &GeneratorCatalogEntry,
    fetcher: &dyn AssetFetcher,
    notice: &mut dyn Write,
) -> Result<InstallReport, InstallError> {
    let dir = layout.model_dir(entry.model_id);
    let mut report = InstallReport {
        model_id: entry.model_id.to_string(),
        ..InstallReport::default()
    };

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

        if target.is_file() && file_matches(&target, file)? {
            report.reused.push(file.relative_path.to_string());
            continue;
        }

        let bytes = download_verified(fetcher, entry, file, &target)?;
        report.downloaded.push(file.relative_path.to_string());
        report.bytes_downloaded += bytes;

        // Crash point *after* a file is durably in place — the resume test
        // kills here and asserts the next run keeps it and continues.
        #[cfg(feature = "failpoints")]
        local_rag_test_support::fail_point!(
            "generate.install.between_files",
            Err(InstallError::Interrupted)
        );
    }

    let manifest = GeneratorManifest {
        model_id: entry.model_id.to_string(),
        source: entry.source.to_string(),
        revision: entry.revision.to_string(),
        license: entry.license.to_string(),
        license_url: entry.license_url.to_string(),
        context_length: entry.context_length,
        files: entry
            .files
            .iter()
            .map(|f| GeneratorManifestFile {
                path: f.relative_path.to_string(),
                size: f.size,
                sha256: f.sha256.to_string(),
            })
            .collect(),
    };
    atomic_write(&dir.join(MANIFEST_FILE), manifest.to_json().as_bytes())?;

    atomic_write(&dir.join(OK_MARKER), b"")?;
    report.marked_ready = true;
    Ok(report)
}

fn download_verified(
    fetcher: &dyn AssetFetcher,
    entry: &GeneratorCatalogEntry,
    file: &AssetFile,
    target: &Path,
) -> Result<u64, InstallError> {
    let part = part_path(target);
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

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(PART_SUFFIX);
    target.with_file_name(name)
}

fn file_matches(path: &Path, file: &AssetFile) -> Result<bool, InstallError> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != file.size {
        return Ok(false);
    }
    Ok(sha256_file(path)? == file.sha256)
}

pub(crate) fn sha256_file(path: &Path) -> io::Result<String> {
    let mut reader = File::open(path)?;
    let mut hasher = HashingWriter::in_memory();
    io::copy(&mut reader, &mut hasher)?;
    Ok(hasher.digest())
}

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

fn sync_dir(dir: &Path) -> io::Result<()> {
    match File::open(dir) {
        Ok(handle) => handle.sync_all(),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        Err(e) => Err(e),
    }
}

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
        assert_eq!(
            part_path(Path::new("/m/model.gguf")),
            Path::new("/m/model.gguf.part")
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
