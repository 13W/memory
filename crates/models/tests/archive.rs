//! `local_rag_models::archive` — T22-15.
//!
//! Every fixture here is built byte by byte in the test rather than committed,
//! for the reason `npm/memory/test/archive.test.js` gives for its own goldens:
//! an archive checked into a repository is a claim about what some `tar` did on
//! some machine once, and the interesting cases (a truncated member, a bad CRC,
//! a pax header) are precisely the ones no ordinary tool will produce on
//! request. Building them makes the shape of each case readable.
//!
//! What is NOT here, deliberately: the real ONNX Runtime archives. They are
//! 8–77 MB, they need the network, and `crates/models/src/ort_catalog.rs`'s
//! pins are what assert against them — verified once, by extracting all five
//! and comparing digests, and recorded in T22-15's evidence.

use std::io::Write;
use std::path::{Path, PathBuf};

use local_rag_models::archive::{ArchiveError, ArchiveFormat, Limits, extract_member};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

const REG: u8 = b'0';
const PAX: u8 = b'x';
const GNU_LONGNAME: u8 = b'L';
const DIR: u8 = b'5';

struct TarMember<'a> {
    name: &'a str,
    typeflag: u8,
    data: &'a [u8],
}

/// A 512-byte ustar header with a correct checksum.
fn tar_header(m: &TarMember) -> [u8; 512] {
    let mut h = [0u8; 512];
    let name = m.name.as_bytes();
    h[..name.len()].copy_from_slice(name);
    h[100..107].copy_from_slice(b"0000644");
    h[108..115].copy_from_slice(b"0000000");
    h[116..123].copy_from_slice(b"0000000");
    let size = format!("{:011o} ", m.data.len());
    h[124..124 + size.len()].copy_from_slice(size.as_bytes());
    h[136..147].copy_from_slice(b"00000000000");
    h[156] = m.typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // The checksum is computed with its own field read as eight spaces.
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let chk = format!("{sum:06o}\0 ");
    h[148..148 + chk.len()].copy_from_slice(chk.as_bytes());
    h
}

fn tar_gz(members: &[TarMember]) -> Vec<u8> {
    let mut tar = Vec::new();
    for m in members {
        tar.extend_from_slice(&tar_header(m));
        tar.extend_from_slice(m.data);
        let pad = (512 - m.data.len() % 512) % 512;
        tar.extend(std::iter::repeat_n(0u8, pad));
    }
    // Two zero blocks: the end-of-archive marker.
    tar.extend(std::iter::repeat_n(0u8, 1024));
    gzip(&tar)
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).expect("gzip a fixture");
    e.finish().expect("finish gzip")
}

struct ZipMember<'a> {
    name: &'a str,
    data: &'a [u8],
    /// 0 = stored, 8 = deflate.
    method: u16,
    /// Overrides the real CRC-32, for the corruption case.
    crc_override: Option<u32>,
    /// Sets the "encrypted" general-purpose flag.
    encrypted: bool,
}

impl<'a> ZipMember<'a> {
    fn deflated(name: &'a str, data: &'a [u8]) -> Self {
        ZipMember {
            name,
            data,
            method: 8,
            crc_override: None,
            encrypted: false,
        }
    }
    fn stored(name: &'a str, data: &'a [u8]) -> Self {
        ZipMember {
            name,
            data,
            method: 0,
            crc_override: None,
            encrypted: false,
        }
    }
}

fn zip(members: &[ZipMember]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for m in members {
        let payload = if m.method == 8 {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(m.data).expect("deflate a fixture");
            e.finish().expect("finish deflate")
        } else {
            m.data.to_vec()
        };
        let mut crc = flate2::Crc::new();
        crc.update(m.data);
        let crc = m.crc_override.unwrap_or_else(|| crc.sum());
        let flags: u16 = if m.encrypted { 1 } else { 0 };
        let offset = out.len() as u32;

        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&m.method.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // time+date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(m.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(m.name.len() as u16).to_le_bytes());
        // A local extra field the central directory does not have — the exact
        // asymmetry `archive.rs` says it handles, present in every fixture so
        // the zip path is never tested without it.
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(m.name.as_bytes());
        out.extend_from_slice(b"XTRA");
        out.extend_from_slice(&payload);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&flags.to_le_bytes());
        central.extend_from_slice(&m.method.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        central.extend_from_slice(&(m.data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(m.name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // no extra field here
        central.extend_from_slice(&0u16.to_le_bytes()); // no comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(m.name.as_bytes());
    }

    let cd_offset = out.len() as u32;
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // no comment
    out
}

struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("lr-archive-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Tmp(dir)
    }
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).expect("write fixture");
        p
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn extract(path: &Path, format: ArchiveFormat, member: &str) -> Result<Vec<u8>, ArchiveError> {
    extract_with(path, format, member, &Limits::default())
}

fn extract_with(
    path: &Path,
    format: ArchiveFormat,
    member: &str,
    limits: &Limits,
) -> Result<Vec<u8>, ArchiveError> {
    let mut out = Vec::new();
    let n = extract_member(path, format, member, &mut out, limits)?;
    assert_eq!(
        n as usize,
        out.len(),
        "the reported count must match what was written"
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// tar.gz
// ---------------------------------------------------------------------------

#[test]
fn a_tar_member_is_extracted_past_its_neighbours() {
    let t = Tmp::new("tar-basic");
    let wanted = vec![7u8; 5000]; // spans several 512-byte blocks, unaligned
    let a = t.write(
        "a.tgz",
        &tar_gz(&[
            TarMember {
                name: "pkg/lib/",
                typeflag: DIR,
                data: b"",
            },
            TarMember {
                name: "pkg/first",
                typeflag: REG,
                data: b"not this one",
            },
            TarMember {
                name: "pkg/lib/target",
                typeflag: REG,
                data: &wanted,
            },
            TarMember {
                name: "pkg/last",
                typeflag: REG,
                data: b"nor this",
            },
        ]),
    );
    assert_eq!(
        extract(&a, ArchiveFormat::TarGz, "pkg/lib/target").expect("found"),
        wanted
    );
}

#[test]
fn a_pax_header_before_the_member_is_skipped_rather_than_confused_for_it() {
    // macOS `bsdtar` emits one before every regular member, because every file
    // there carries an unremovable `com.apple.provenance` xattr. A reader that
    // admitted only typeflag `0` would call the most ordinary archive corrupt.
    let t = Tmp::new("tar-pax");
    let a = t.write(
        "a.tgz",
        &tar_gz(&[
            TarMember {
                name: "PaxHeader/pkg/lib",
                typeflag: PAX,
                data: b"30 comment=irrelevant\n",
            },
            TarMember {
                name: "pkg/lib",
                typeflag: REG,
                data: b"the real bytes",
            },
        ]),
    );
    assert_eq!(
        extract(&a, ArchiveFormat::TarGz, "pkg/lib").expect("found"),
        b"the real bytes"
    );
}

#[test]
fn a_leading_dot_slash_is_normalised_on_both_sides() {
    // ONNX Runtime's macOS archives store `./pkg/...` and its Linux ones store
    // `pkg/...`, for the same release. The pin names the file; `./` names the
    // writer. Found by extracting the real archives, not by reading a spec.
    let t = Tmp::new("tar-dotslash");
    let a = t.write(
        "a.tgz",
        &tar_gz(&[TarMember {
            name: "./pkg/lib/target",
            typeflag: REG,
            data: b"macos-style",
        }]),
    );
    assert_eq!(
        extract(&a, ArchiveFormat::TarGz, "pkg/lib/target").expect("found"),
        b"macos-style"
    );
    assert_eq!(
        extract(&a, ArchiveFormat::TarGz, "./pkg/lib/target").expect("found"),
        b"macos-style"
    );
}

#[test]
fn a_gnu_long_name_entry_is_refused_rather_than_guessed() {
    // Its payload is the *next* member's name; mishandling one silently shifts
    // every subsequent name by one, which is worse than not reading the archive.
    let t = Tmp::new("tar-gnu");
    let a = t.write(
        "a.tgz",
        &tar_gz(&[
            TarMember {
                name: "././@LongLink",
                typeflag: GNU_LONGNAME,
                data: b"a/very/long/name\0",
            },
            TarMember {
                name: "a/very/long/name",
                typeflag: REG,
                data: b"payload",
            },
        ]),
    );
    assert!(matches!(
        extract(&a, ArchiveFormat::TarGz, "a/very/long/name"),
        Err(ArchiveError::Unsupported { .. })
    ));
}

#[test]
fn a_directory_entry_of_the_right_name_is_not_the_member() {
    let t = Tmp::new("tar-dir");
    let a = t.write(
        "a.tgz",
        &tar_gz(&[TarMember {
            name: "pkg/lib",
            typeflag: DIR,
            data: b"",
        }]),
    );
    assert!(matches!(
        extract(&a, ArchiveFormat::TarGz, "pkg/lib"),
        Err(ArchiveError::MemberMissing { .. })
    ));
}

#[test]
fn a_tar_that_ends_inside_its_member_is_an_error_not_a_short_file() {
    // The failure that would otherwise become a silently truncated install.
    let t = Tmp::new("tar-truncated");
    let full = tar_gz(&[TarMember {
        name: "pkg/lib",
        typeflag: REG,
        data: &vec![3u8; 4096],
    }]);
    // Re-gzip a tar cut off mid-member, so the gzip stream itself stays valid
    // and the truncation is the tar's, not the container's.
    let mut raw = Vec::new();
    {
        use std::io::Read;
        flate2::read::GzDecoder::new(&full[..])
            .read_to_end(&mut raw)
            .expect("ungzip");
    }
    raw.truncate(512 + 1000);
    let a = t.write("a.tgz", &gzip(&raw));
    let err = extract(&a, ArchiveFormat::TarGz, "pkg/lib").expect_err("truncated");
    assert!(format!("{err}").contains("ends inside a member"), "{err}");
}

#[test]
fn a_member_over_the_ceiling_is_refused_before_it_is_read() {
    let t = Tmp::new("tar-ceiling");
    let a = t.write(
        "a.tgz",
        &tar_gz(&[TarMember {
            name: "pkg/lib",
            typeflag: REG,
            data: &vec![1u8; 4096],
        }]),
    );
    let limits = Limits {
        max_archive_bytes: 1 << 20,
        max_member_bytes: 4095,
    };
    assert!(matches!(
        extract_with(&a, ArchiveFormat::TarGz, "pkg/lib", &limits),
        Err(ArchiveError::TooLarge { .. })
    ));
}

#[test]
fn an_archive_over_the_ceiling_is_refused_before_it_is_opened() {
    let t = Tmp::new("archive-ceiling");
    let a = t.write(
        "a.tgz",
        &tar_gz(&[TarMember {
            name: "pkg/lib",
            typeflag: REG,
            data: b"small",
        }]),
    );
    let limits = Limits {
        max_archive_bytes: 1,
        max_member_bytes: 1 << 20,
    };
    assert!(matches!(
        extract_with(&a, ArchiveFormat::TarGz, "pkg/lib", &limits),
        Err(ArchiveError::TooLarge { .. })
    ));
}

// ---------------------------------------------------------------------------
// zip
// ---------------------------------------------------------------------------

#[test]
fn a_deflated_zip_member_is_extracted_and_its_crc_checked() {
    let t = Tmp::new("zip-deflate");
    let wanted: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let a = t.write(
        "a.zip",
        &zip(&[
            ZipMember::deflated("pkg/other", b"not this"),
            ZipMember::deflated("pkg/lib/target", &wanted),
        ]),
    );
    assert_eq!(
        extract(&a, ArchiveFormat::Zip, "pkg/lib/target").expect("found"),
        wanted
    );
}

#[test]
fn a_stored_zip_member_is_extracted_too() {
    let t = Tmp::new("zip-stored");
    let a = t.write(
        "a.zip",
        &zip(&[ZipMember::stored("pkg/lib", b"uncompressed bytes")]),
    );
    assert_eq!(
        extract(&a, ArchiveFormat::Zip, "pkg/lib").expect("found"),
        b"uncompressed bytes"
    );
}

#[test]
fn a_zip_member_whose_crc_does_not_match_is_rejected_after_extraction() {
    // gzip carries its own checksum and `GzDecoder` enforces it, so only the
    // zip path needs this. Without it a corrupt member is plausible bytes.
    let t = Tmp::new("zip-crc");
    let mut m = ZipMember::deflated("pkg/lib", b"the real bytes");
    m.crc_override = Some(0xdead_beef);
    let a = t.write("a.zip", &zip(&[m]));
    let err = extract(&a, ArchiveFormat::Zip, "pkg/lib").expect_err("bad crc");
    assert!(format!("{err}").contains("CRC-32"), "{err}");
}

#[test]
fn an_encrypted_zip_member_is_refused_rather_than_decompressed_into_noise() {
    let t = Tmp::new("zip-encrypted");
    let mut m = ZipMember::stored("pkg/lib", b"whatever");
    m.encrypted = true;
    let a = t.write("a.zip", &zip(&[m]));
    assert!(matches!(
        extract(&a, ArchiveFormat::Zip, "pkg/lib"),
        Err(ArchiveError::Unsupported { .. })
    ));
}

#[test]
fn a_zip_member_that_is_not_there_says_so() {
    let t = Tmp::new("zip-missing");
    let a = t.write("a.zip", &zip(&[ZipMember::deflated("pkg/other", b"x")]));
    assert!(matches!(
        extract(&a, ArchiveFormat::Zip, "pkg/lib"),
        Err(ArchiveError::MemberMissing { .. })
    ));
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

#[test]
fn the_declared_format_must_match_the_bytes() {
    // The catalog says which container an asset ships in. Checking the magic
    // against that declaration catches a pin that has drifted from the release
    // — a reader that trusted the declaration would feed a zip to a gzip
    // decoder and report a decompression error instead.
    let t = Tmp::new("dispatch");
    let tar = t.write(
        "a.tgz",
        &tar_gz(&[TarMember {
            name: "m",
            typeflag: REG,
            data: b"x",
        }]),
    );
    let z = t.write("a.zip", &zip(&[ZipMember::stored("m", b"x")]));

    let err = extract(&tar, ArchiveFormat::Zip, "m").expect_err("tar is not a zip");
    assert!(format!("{err}").contains("declared as Zip"), "{err}");
    let err = extract(&z, ArchiveFormat::TarGz, "m").expect_err("zip is not a tar.gz");
    assert!(format!("{err}").contains("declared as TarGz"), "{err}");
}

#[test]
fn a_file_too_short_to_be_an_archive_is_a_format_error() {
    let t = Tmp::new("short");
    let a = t.write("a.tgz", b"hi");
    assert!(matches!(
        extract(&a, ArchiveFormat::TarGz, "m"),
        Err(ArchiveError::Format { .. })
    ));
}
