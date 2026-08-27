//! The pinned ONNX Runtime catalog
//!
//! Named `ort_catalog` rather than `ort` on purpose: this crate also
//! depends on the `ort` crate, and a module of the same name would leave
//! every bare `ort::` path in `onnx.rs` resolving by a rule the reader has
//! to know rather than see. — the single source of truth for which
//! shared library each platform gets, where it comes from, and what it must
//! hash to.
//!
//! # Why it lives here and not in `xtask`
//!
//! It used to be `crates/xtask/src/dist_ort.rs::ORT_ASSETS`, back when the
//! library was bundled into a per-platform npm package by a release tool
//! (T17-03). ADR-0013 made it an artifact of first run instead, installed into
//! the store beside the model weights (spec 10 §5 `[FIXED]`), so the *runtime*
//! now needs the catalog. `xtask` already depends on this crate; the reverse
//! edge would be a cycle, so the table moves here and `dist-ort` reads it from
//! here. There is still exactly one copy.
//!
//! # The pins are the security boundary, and they are not relaxed
//!
//! Spec 13 §2 says so explicitly: under `latest` the product binaries lose the
//! compiled-in-digest standard (ADR-0013 §Decision 2) and **the runtime
//! deliberately does not**. Every URL and digest below was re-fetched and
//! re-hashed when this table moved (T22-15); all four pre-existing archive
//! digests matched their previous values byte for byte, which is what an
//! immutable release asset should do and is worth having checked rather than
//! assumed.

use crate::archive::ArchiveFormat;

/// One platform's pinned ONNX Runtime release archive and the library inside.
#[derive(Debug, Clone, Copy)]
pub struct OrtAsset {
    /// Platform key, in this project's own `<os>-<arch>` convention.
    pub platform: &'static str,
    /// The upstream ONNX Runtime release tag this asset was fetched from.
    pub version: &'static str,
    /// The exact release-asset download URL.
    pub url: &'static str,
    /// Which container the asset ships in.
    pub archive_format: ArchiveFormat,
    /// The archive's own size in bytes.
    pub archive_size: u64,
    /// The archive's pinned SHA-256, checked before anything is unpacked.
    pub archive_sha256: &'static str,
    /// Path, inside the archive, to the *real* (non-symlink) library file.
    ///
    /// ONNX Runtime's own release layout ships the fully versioned file name
    /// (`libonnxruntime.<version>.dylib`/`.so.<version>`) as a regular file and
    /// the unversioned name as a symlink to it — confirmed by inspecting all
    /// four archives, where the unversioned entry is a symlink in three and, in
    /// the fourth, a byte-identical duplicate. Always targeting the versioned
    /// member means extraction never depends on a symlink surviving.
    ///
    /// Written without a leading `./` even though the macOS archives store one:
    /// see [`crate::archive`]'s `normalize` for why that difference belongs to
    /// the writer rather than to the file.
    pub archive_member: &'static str,
    /// The flat file name the loader looks for
    /// (`crates/models/src/onnx.rs::ort_dylib_file_name`).
    pub dylib_name: &'static str,
    /// The extracted library's own size in bytes.
    pub dylib_size: u64,
    /// The extracted library's own SHA-256.
    ///
    /// A second pin, and not redundant with `archive_sha256`: that one says the
    /// bytes on the wire were the right archive, this one says the bytes taken
    /// *out of it* are the right file. The extractor is this project's own
    /// code, and an installer that verified only its input would place whatever
    /// its own bug produced.
    pub dylib_sha256: &'static str,
}

/// Every platform with a pinned runtime.
///
/// `darwin-x64` pins an older release than the others on purpose: ONNX Runtime
/// dropped prebuilt Intel-Mac binaries as of v1.27.0, and v1.20.0 is the newest
/// tag that still ships `onnxruntime-osx-x86_64-*`. The other three pin the
/// release this project's own G11 gate validated end to end.
///
/// `win32-x64` joined in T22-15. Its absence used to be blamed on
/// `cargo-zigbuild`, which has had no role since ADR-0013; with the bundling
/// step gone the Windows archive is just another download, and `D-108` had
/// already established that Windows *product* binaries ship. `win32-arm64`
/// stays deferred `[FIXED]` (spec 13 §1) — no product binary, so nothing to
/// pair a runtime with.
pub const ORT_ASSETS: &[OrtAsset] = &[
    OrtAsset {
        platform: "darwin-arm64",
        version: "1.27.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-osx-arm64-1.27.0.tgz",
        archive_format: ArchiveFormat::TarGz,
        archive_size: 32_485_368,
        archive_sha256: "545e81c58152353acb0d1e8bd6ce4b62f830c0961f5b3acfedc790ffd76e477a",
        archive_member: "onnxruntime-osx-arm64-1.27.0/lib/libonnxruntime.1.27.0.dylib",
        dylib_name: "libonnxruntime.dylib",
        dylib_size: 38_313_360,
        dylib_sha256: "299e5a2c6ea00531ecd6bf3217e23798c1fcba1698bb386d313c8e16d7317d60",
    },
    OrtAsset {
        platform: "darwin-x64",
        version: "1.20.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.20.0/onnxruntime-osx-x86_64-1.20.0.tgz",
        archive_format: ArchiveFormat::TarGz,
        archive_size: 8_940_384,
        archive_sha256: "d28e603b47b74050f2c30a7069bf3fb371cfba7205d7771f22cabc7b02953757",
        archive_member: "onnxruntime-osx-x86_64-1.20.0/lib/libonnxruntime.1.20.0.dylib",
        dylib_name: "libonnxruntime.dylib",
        dylib_size: 28_600_008,
        dylib_sha256: "542ffd4568821088ff3e42a3aa19c37dbbd73b522bfe58505520de332e581b4d",
    },
    OrtAsset {
        platform: "linux-x64",
        version: "1.27.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-linux-x64-1.27.0.tgz",
        archive_format: ArchiveFormat::TarGz,
        archive_size: 8_831_605,
        archive_sha256: "547e40a48f1fe73e3f812d7c88a948612c23f896b91e4e2ee1e232d7b468246f",
        archive_member: "onnxruntime-linux-x64-1.27.0/lib/libonnxruntime.so.1.27.0",
        dylib_name: "libonnxruntime.so",
        dylib_size: 23_658_512,
        dylib_sha256: "4061866361d9a8d2872f5f419c5515ce35a830a0c5c77ce1723320ac0dbabfc7",
    },
    OrtAsset {
        platform: "linux-arm64",
        version: "1.27.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-linux-aarch64-1.27.0.tgz",
        archive_format: ArchiveFormat::TarGz,
        archive_size: 7_797_972,
        archive_sha256: "3e4d83ac06924a32a07b6d7f91ce6f852876153fc0bbdf931bf517a140bfbe48",
        archive_member: "onnxruntime-linux-aarch64-1.27.0/lib/libonnxruntime.so.1.27.0",
        dylib_name: "libonnxruntime.so",
        dylib_size: 20_001_880,
        dylib_sha256: "c36bc200e7e6c093b8abb0b34590fbdd8c52fd3fb5e33795b9523ffdde2fce0f",
    },
    OrtAsset {
        platform: "win32-x64",
        version: "1.27.0",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.27.0/onnxruntime-win-x64-1.27.0.zip",
        archive_format: ArchiveFormat::Zip,
        archive_size: 77_086_915,
        archive_sha256: "c5c81710938e68079ff1a192b04897faabe4b43830d48f39f27ecd4e16138bfc",
        // The archive also carries a 403 MB `onnxruntime.pdb` next to this
        // 15 MB DLL — the reason the extractor streams one member rather than
        // unpacking, stated where the size is visible.
        archive_member: "onnxruntime-win-x64-1.27.0/lib/onnxruntime.dll",
        dylib_name: "onnxruntime.dll",
        dylib_size: 15_381_816,
        dylib_sha256: "fd6dd0a8b1f5562d642abdcbd36bc54251482d2ebaa3f4f88669bfdad92e7525",
    },
];

/// This build's own platform key, in the catalog's convention.
///
/// Derived from `cfg!` rather than from a runtime string so a build can only
/// ever ask for the runtime it could actually load.
pub fn current_platform() -> Option<&'static str> {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else {
        return None;
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        return None;
    };
    Some(match (os, arch) {
        ("darwin", "arm64") => "darwin-arm64",
        ("darwin", "x64") => "darwin-x64",
        ("linux", "arm64") => "linux-arm64",
        ("linux", "x64") => "linux-x64",
        ("win32", "x64") => "win32-x64",
        _ => return None,
    })
}

/// Find a catalog entry by its platform key.
pub fn find(platform: &str) -> Option<&'static OrtAsset> {
    ORT_ASSETS.iter().find(|a| a.platform == platform)
}

/// The entry for this build's own platform, if it has one.
pub fn for_current_platform() -> Option<&'static OrtAsset> {
    current_platform().and_then(find)
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
                "win32-x64",
            ]),
            "win32-arm64 is deliberately absent — there is no product binary to pair with it"
        );
    }

    #[test]
    fn every_pinned_digest_is_a_64_character_hex_string() {
        // Both of them. The library digest is the newer pin and the easier one
        // to paste wrong, since nothing upstream publishes it.
        for asset in ORT_ASSETS {
            for (what, digest) in [
                ("archive", asset.archive_sha256),
                ("dylib", asset.dylib_sha256),
            ] {
                assert_eq!(
                    digest.len(),
                    64,
                    "{}: {what} is not a sha256",
                    asset.platform
                );
                assert!(
                    digest.chars().all(|c| c.is_ascii_hexdigit()),
                    "{}: {what} is not a sha256",
                    asset.platform
                );
            }
            assert_ne!(
                asset.archive_sha256, asset.dylib_sha256,
                "{}: the two digests cannot be the same file",
                asset.platform
            );
        }
    }

    #[test]
    fn every_pinned_size_is_plausible() {
        // Not a tight bound — a wrong-by-a-lot size is the shape of a
        // copy/paste slip, and a zero would make the installer's size check
        // vacuous.
        for asset in ORT_ASSETS {
            assert!(asset.archive_size > 1_000_000, "{}", asset.platform);
            assert!(asset.dylib_size > 1_000_000, "{}", asset.platform);
        }
    }

    #[test]
    fn the_declared_container_matches_the_url_suffix() {
        for asset in ORT_ASSETS {
            let expected = if asset.url.ends_with(".zip") {
                ArchiveFormat::Zip
            } else {
                ArchiveFormat::TarGz
            };
            assert_eq!(asset.archive_format, expected, "{}", asset.platform);
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
                .trim_end_matches(".tgz")
                .trim_end_matches(".zip");
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
    fn each_platform_names_its_library_the_way_that_platform_loads_it() {
        // The same three names `crates/models/src/onnx.rs::ort_dylib_file_name`
        // resolves at run time — a mismatch would install a file the loader
        // then does not look for.
        for asset in ORT_ASSETS {
            let expected = if asset.platform.starts_with("darwin") {
                "libonnxruntime.dylib"
            } else if asset.platform.starts_with("win32") {
                "onnxruntime.dll"
            } else {
                "libonnxruntime.so"
            };
            assert_eq!(asset.dylib_name, expected, "{}", asset.platform);
        }
    }

    #[test]
    fn find_resolves_known_platforms_and_rejects_unknown_ones() {
        assert!(find("darwin-arm64").is_some());
        assert!(find("win32-x64").is_some(), "added by T22-15");
        assert!(
            find("win32-arm64").is_none(),
            "deferred [FIXED], spec 13 §1"
        );
        assert!(find("freebsd-x64").is_none());
    }

    #[test]
    fn this_host_resolves_to_an_entry_it_could_actually_load() {
        // `current_platform` is derived from `cfg!`, so on every platform this
        // project supports it must name a real entry; anywhere else it must say
        // so rather than guess.
        match current_platform() {
            Some(key) => {
                let asset = find(key).unwrap_or_else(|| panic!("{key} has no catalog entry"));
                assert_eq!(
                    for_current_platform().map(|a| a.platform),
                    Some(asset.platform)
                );
            }
            None => assert!(for_current_platform().is_none()),
        }
    }
}
