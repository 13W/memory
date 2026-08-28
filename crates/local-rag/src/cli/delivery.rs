//! Where the running binary itself came from — `doctor`'s `delivery:` section
//! (T22-16, spec 13 §1/§2).
//!
//! ADR-0013 made the product binaries GitHub release assets, obtained by the
//! `@13w/memory` npm package, which records what it installed in a manifest
//! beside them: `.local-rag-install.json`. Its JS reader
//! (`npm/memory/src/locate.js::installInfo`) already carries the words
//! "`doctor` (T22-16) reads this" in its doc comment; this is that reader, on
//! the Rust side, for the one directory that matters here.
//!
//! # Only the directory this executable is in
//!
//! `installInfo()` walks the whole resolution ladder because it answers "which
//! installation *would* be used". This answers a narrower and more useful
//! question for a diagnostic: **where did the binary that is running right now
//! come from**. `current_exe()` is the only honest anchor for that, and a
//! ladder walk could easily describe an installation this process is not from.
//!
//! # A missing manifest is not a fault
//!
//! It means the binary was not installed by the npm package: a source checkout
//! (`cargo run`, `target/release`), a hand-placed `LOCAL_RAG_BIN_DIR`, a
//! distribution package. All legitimate, none diagnosable from here, so the
//! section says which of "managed" or "unmanaged" applies and stops.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The file `npm/memory/src/install.js` writes next to the binaries it placed.
pub const INSTALL_MANIFEST: &str = ".local-rag-install.json";

/// The only `manifestVersion` this reader understands — the writer's own
/// `MANIFEST_VERSION` (`npm/memory/src/install.js`).
///
/// Checked rather than ignored: every other field's *meaning* is defined by
/// this number, so reporting a tag and a platform out of a manifest from some
/// future shape would be stating facts this code cannot actually vouch for.
/// The installer applies the same rule in the other direction —
/// `manifestIsCurrent` refuses a manifest whose version it does not know.
pub const SUPPORTED_MANIFEST_VERSION: u32 = 1;

/// One binary's entry in the manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct ManifestBinary {
    /// `"installed"` or `"absent"` — the release may legitimately not carry an
    /// optional binary, which the installer records rather than failing on.
    pub state: String,
    /// Present only for `state == "installed"`.
    #[serde(default)]
    pub file: Option<String>,
}

/// `.local-rag-install.json`, as the npm installer writes it.
///
/// Field names are `camelCase` on disk because the writer is JavaScript; the
/// rename is `serde`'s, not a re-spelling of the format.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallManifest {
    pub manifest_version: u32,
    pub package_version: String,
    pub platform_key: String,
    #[serde(default)]
    pub target_triple: Option<String>,
    /// The release tag the binaries were taken from — the single most useful
    /// fact here, since `latest` moves and a stale install is otherwise silent.
    pub tag: String,
    #[serde(default)]
    pub binaries: std::collections::BTreeMap<String, ManifestBinary>,
}

/// What `doctor` reports about this binary's provenance.
#[derive(Debug)]
pub enum DeliveryFinding {
    /// `current_exe()` itself failed — vanishingly rare, but reporting it
    /// beats printing a confident "unmanaged".
    Unknown { detail: String },
    /// No manifest beside the executable: a checkout or a hand-placed binary.
    Unmanaged { exe_dir: PathBuf },
    /// A manifest is there but could not be read or parsed. Worth saying out
    /// loud: it means an installed layout whose own record is damaged.
    Damaged { path: PathBuf, detail: String },
    Managed {
        exe_dir: PathBuf,
        manifest: Box<InstallManifest>,
    },
}

/// Read the manifest beside the running executable.
pub fn diagnose() -> DeliveryFinding {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            return DeliveryFinding::Unknown {
                detail: e.to_string(),
            };
        }
    };
    let dir = match exe.parent() {
        Some(dir) => dir.to_path_buf(),
        None => {
            return DeliveryFinding::Unknown {
                detail: format!("{} has no parent directory", exe.display()),
            };
        }
    };
    diagnose_in(&dir)
}

/// The directory-scoped half, so a test can point it anywhere without moving
/// the test binary.
pub fn diagnose_in(dir: &Path) -> DeliveryFinding {
    let path = dir.join(INSTALL_MANIFEST);
    if !path.is_file() {
        return DeliveryFinding::Unmanaged {
            exe_dir: dir.to_path_buf(),
        };
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            return DeliveryFinding::Damaged {
                path,
                detail: e.to_string(),
            };
        }
    };
    match serde_json::from_str::<InstallManifest>(&text) {
        Ok(manifest) if manifest.manifest_version != SUPPORTED_MANIFEST_VERSION => {
            DeliveryFinding::Damaged {
                path,
                detail: format!(
                    "manifestVersion {} is not the {} this binary understands — the installed                      layout is newer or older than this executable",
                    manifest.manifest_version, SUPPORTED_MANIFEST_VERSION,
                ),
            }
        }
        Ok(manifest) => DeliveryFinding::Managed {
            exe_dir: dir.to_path_buf(),
            manifest: Box::new(manifest),
        },
        Err(e) => DeliveryFinding::Damaged {
            path,
            detail: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest in exactly the shape `npm/memory/src/install.js` writes —
    /// `camelCase`, `manifestVersion` first, per-binary `state`/`file`. Typed
    /// out rather than generated, because the point of these tests is that this
    /// reader agrees with that writer's format.
    const REAL_SHAPE: &str = r#"{
  "manifestVersion": 1,
  "packageVersion": "0.0.0",
  "platformKey": "darwin-arm64",
  "targetTriple": "aarch64-apple-darwin",
  "tag": "0.0.0",
  "binaries": {
    "local-rag": { "state": "installed", "file": "local-rag", "archiveSha256": "aa" },
    "local-rag-tui": { "state": "absent" }
  }
}
"#;

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lr-delivery-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_real_shaped_manifest_parses_including_the_absent_binary() {
        let dir = tmp("real");
        std::fs::write(dir.join(INSTALL_MANIFEST), REAL_SHAPE).expect("write");
        let DeliveryFinding::Managed { manifest, .. } = diagnose_in(&dir) else {
            panic!("expected a managed layout");
        };
        assert_eq!(manifest.tag, "0.0.0");
        assert_eq!(manifest.platform_key, "darwin-arm64");
        assert_eq!(
            manifest.target_triple.as_deref(),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(manifest.binaries["local-rag"].state, "installed");
        assert_eq!(
            manifest.binaries["local-rag"].file.as_deref(),
            Some("local-rag")
        );
        // `absent` entries carry no `file`, and must not be a parse failure:
        // the installer records a binary the release did not carry rather than
        // failing, so a reader that rejected it would call a healthy install
        // damaged.
        assert_eq!(manifest.binaries["local-rag-tui"].state, "absent");
        assert!(manifest.binaries["local-rag-tui"].file.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_manifest_version_is_reported_rather_than_read() {
        // Every other field's meaning is defined by this number; reporting a
        // tag out of a manifest shape this code does not know would be stating
        // a fact it cannot vouch for.
        let dir = tmp("version");
        let text = REAL_SHAPE.replace("\"manifestVersion\": 1", "\"manifestVersion\": 99");
        std::fs::write(dir.join(INSTALL_MANIFEST), text).expect("write");
        let DeliveryFinding::Damaged { detail, .. } = diagnose_in(&dir) else {
            panic!("expected a damaged report");
        };
        assert!(detail.contains("99"), "{detail}");
        assert!(detail.contains("newer or older"), "{detail}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unparseable_json_is_damaged_not_unmanaged() {
        // The distinction matters to whoever reads the line: "no manifest" is
        // normal, "a manifest that will not parse" is an installed layout whose
        // own record is broken.
        let dir = tmp("garbage");
        std::fs::write(dir.join(INSTALL_MANIFEST), "{ not json").expect("write");
        assert!(matches!(diagnose_in(&dir), DeliveryFinding::Damaged { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_directory_without_a_manifest_is_unmanaged() {
        let dir = tmp("bare");
        assert!(matches!(
            diagnose_in(&dir),
            DeliveryFinding::Unmanaged { .. }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
