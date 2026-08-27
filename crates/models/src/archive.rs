//! Extract exactly one member from a `.tar.gz` or a `.zip` — T22-15.
//!
//! ONNX Runtime publishes its shared library only inside its own release
//! archives, so installing the runtime at first run (spec 10 §5 `[FIXED,
//! ADR-0013]`) means opening one. `cargo xtask dist-ort` did the same job by
//! shelling out to the system `tar`, which is fine for a manually invoked dev
//! tool and not for product code — see [`crate::fetch`]'s own doc comment for
//! why this crate does not shell out.
//!
//! # This is the Rust twin of `npm/memory/src/archive.js`
//!
//! That file (T22-07) reads the same two container formats for the same
//! project, and every decision below that looks arbitrary was paid for there by
//! inspecting real archive bytes rather than reading a format spec:
//!
//! - **Dispatch is checked, not inferred.** The caller declares the format (it
//!   comes from a pinned catalog entry), and the magic bytes must agree. A
//!   reader that trusted the file name would be one rename away from garbage;
//!   one that only sniffed would silently accept an archive the pin did not
//!   describe.
//! - **pax extended headers are skipped unread** (`typeflag` `x`/`g`). macOS
//!   `bsdtar` emits one before every regular member, because every file there
//!   carries an unremovable `com.apple.provenance` xattr — a tar reader that
//!   admits only `0`/`\0` sees a "corrupt" archive on the most ordinary input.
//! - **GNU long names are rejected, not guessed** (`typeflag` `L`/`K`). Their
//!   payload is the *next* member's name, so mishandling one silently shifts
//!   every subsequent name by one.
//! - **A zip's member name and extra-field length come from the local header,
//!   its offsets and sizes from the central directory.** The two extra-field
//!   lengths routinely differ, and using the central one to skip past the local
//!   header lands mid-data.
//!
//! # Streaming, unlike the JS twin
//!
//! `archive.js` loads the whole archive into memory and says why: Node's zlib
//! there is buffer-oriented and a zip's index lives in its tail. Here a `File`
//! seeks, so a tar is streamed member by member and a zip is read by seeking to
//! the one member that matters. That is not a nicety: the Windows archive is
//! 77 MB and carries a 403 MB `.pdb` next to the 15 MB DLL, so the difference
//! between "stream one member" and "unpack" is nearly half a gigabyte.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use flate2::read::{DeflateDecoder, GzDecoder};

/// Which container a pinned asset ships in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// gzip-compressed tar — every platform but Windows.
    TarGz,
    /// zip — the Windows archive.
    Zip,
}

/// Size ceilings, so a hostile or corrupt archive cannot exhaust the disk.
///
/// Fields rather than constants because a ceiling nobody can lower is a
/// ceiling no test can reach: proving one fires would otherwise need a
/// multi-gigabyte fixture. The same reason `npm/memory/src/http.js` makes its
/// own limits injectable.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest archive this will open at all.
    pub max_archive_bytes: u64,
    /// Largest single member this will extract.
    pub max_member_bytes: u64,
}

impl Default for Limits {
    /// Generous against the real assets, which is the point: the largest thing
    /// this reads today is a 77 MB zip holding a 403 MB member it skips, and
    /// the biggest member it extracts is 38 MB.
    fn default() -> Self {
        Limits {
            max_archive_bytes: 512 * 1024 * 1024,
            max_member_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Why a member could not be extracted.
#[derive(Debug)]
#[non_exhaustive]
pub enum ArchiveError {
    /// The archive is not the container the caller declared, or is malformed.
    Format {
        /// The archive that failed.
        archive: PathBuf,
        /// What is wrong with it.
        detail: String,
    },
    /// A real archive feature this reader deliberately does not implement.
    Unsupported {
        /// The archive that failed.
        archive: PathBuf,
        /// The feature.
        detail: String,
    },
    /// The named member is not in the archive.
    MemberMissing {
        /// The archive that failed.
        archive: PathBuf,
        /// The member that was looked for.
        member: String,
    },
    /// A ceiling from [`Limits`] was reached.
    TooLarge {
        /// The archive that failed.
        archive: PathBuf,
        /// Which ceiling, and its value.
        detail: String,
    },
    /// Reading or writing failed.
    Io(io::Error),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchiveError::Format { archive, detail } => {
                write!(f, "{}: {detail}", archive.display())
            }
            ArchiveError::Unsupported { archive, detail } => {
                write!(f, "{}: unsupported archive: {detail}", archive.display())
            }
            ArchiveError::MemberMissing { archive, member } => {
                write!(f, "{}: no member named \"{member}\"", archive.display())
            }
            ArchiveError::TooLarge { archive, detail } => {
                write!(f, "{}: {detail}", archive.display())
            }
            ArchiveError::Io(e) => write!(f, "archive I/O failed: {e}"),
        }
    }
}

impl std::error::Error for ArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArchiveError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ArchiveError {
    fn from(e: io::Error) -> Self {
        ArchiveError::Io(e)
    }
}

/// Copy the single member named `member` out of `archive` into `sink`.
///
/// Returns the number of bytes written. The member's own name is matched
/// exactly against the path recorded in the archive — never joined onto
/// anything on disk, so path traversal is structurally impossible here rather
/// than defended against.
pub fn extract_member(
    archive: &Path,
    format: ArchiveFormat,
    member: &str,
    sink: &mut dyn Write,
    limits: &Limits,
) -> Result<u64, ArchiveError> {
    let mut file = File::open(archive)?;
    let size = file.metadata()?.len();
    if size > limits.max_archive_bytes {
        return Err(ArchiveError::TooLarge {
            archive: archive.to_path_buf(),
            detail: format!(
                "archive is {size} bytes, over the {}-byte ceiling",
                limits.max_archive_bytes
            ),
        });
    }
    check_magic(archive, &mut file, format)?;
    file.seek(SeekFrom::Start(0))?;

    match format {
        ArchiveFormat::TarGz => extract_from_tar_gz(archive, file, member, sink, limits),
        ArchiveFormat::Zip => extract_from_zip(archive, file, member, sink, limits),
    }
}

/// The declared format must match what the first bytes actually say.
fn check_magic(archive: &Path, file: &mut File, format: ArchiveFormat) -> Result<(), ArchiveError> {
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .map_err(|_| ArchiveError::Format {
            archive: archive.to_path_buf(),
            detail: "file is too short to be an archive".to_string(),
        })?;
    let ok = match format {
        ArchiveFormat::TarGz => magic[0] == 0x1f && magic[1] == 0x8b,
        // `PK\x03\x04`; an empty zip starts `PK\x05\x06` and holds no member.
        ArchiveFormat::Zip => &magic == b"PK\x03\x04",
    };
    if ok {
        return Ok(());
    }
    Err(ArchiveError::Format {
        archive: archive.to_path_buf(),
        detail: format!(
            "declared as {format:?} but starts with {:02x?}",
            &magic[..2]
        ),
    })
}

// ---------------------------------------------------------------------------
// tar.gz
// ---------------------------------------------------------------------------

const TAR_BLOCK: usize = 512;

fn extract_from_tar_gz(
    archive: &Path,
    file: File,
    member: &str,
    sink: &mut dyn Write,
    limits: &Limits,
) -> Result<u64, ArchiveError> {
    let mut reader = GzDecoder::new(file);
    let mut header = [0u8; TAR_BLOCK];
    let mut zero_blocks = 0u8;

    loop {
        match read_full(&mut reader, &mut header)? {
            // A tar that simply stops is still a tar that lacks the member;
            // saying so beats inventing a corruption diagnosis.
            0 => break,
            n if n < TAR_BLOCK => {
                return Err(ArchiveError::Format {
                    archive: archive.to_path_buf(),
                    detail: "archive ends inside a header block".to_string(),
                });
            }
            _ => {}
        }
        if header.iter().all(|&b| b == 0) {
            zero_blocks += 1;
            // Two consecutive zero blocks are the end-of-archive marker.
            if zero_blocks == 2 {
                break;
            }
            continue;
        }
        zero_blocks = 0;

        let typeflag = header[156];
        let size = parse_octal(&header[124..136]).ok_or_else(|| ArchiveError::Format {
            archive: archive.to_path_buf(),
            detail: "member header has an unparseable size field".to_string(),
        })?;

        if typeflag == b'L' || typeflag == b'K' {
            return Err(ArchiveError::Unsupported {
                archive: archive.to_path_buf(),
                detail: "GNU long-name entries".to_string(),
            });
        }

        let name = tar_member_name(&header);
        let is_regular = typeflag == b'0' || typeflag == 0;
        if is_regular && normalize(&name) == normalize(member) {
            if size > limits.max_member_bytes {
                return Err(ArchiveError::TooLarge {
                    archive: archive.to_path_buf(),
                    detail: format!(
                        "member is {size} bytes, over the {}-byte ceiling",
                        limits.max_member_bytes
                    ),
                });
            }
            let copied = copy_exactly(archive, &mut reader, sink, size)?;
            return Ok(copied);
        }

        // Everything else — pax headers (`x`/`g`), directories, symlinks, the
        // regular members we are not after — is skipped without being read.
        skip(&mut reader, padded(size))?;
    }

    Err(ArchiveError::MemberMissing {
        archive: archive.to_path_buf(),
        member: member.to_string(),
    })
}

/// `name` (0..100), prefixed by `prefix` (345..500) when ustar sets one.
fn tar_member_name(header: &[u8; TAR_BLOCK]) -> String {
    let name = nul_terminated(&header[0..100]);
    let prefix = nul_terminated(&header[345..500]);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

/// Drop a leading `./`, which is a property of the writer and not of the file.
///
/// MEASURED, NOT ASSUMED, AND IT COST A DEBUGGING ROUND. ONNX Runtime's macOS
/// archives store every path as `./onnxruntime-.../lib/...` and its Linux ones
/// store the same paths bare — two producers, one project, one release. The
/// pins in [`crate::ort_catalog::ORT_ASSETS`] carry the bare form and had never been
/// literally correct for macOS: `cargo xtask dist-ort` shells out to the system
/// `tar`, which normalises `./` when matching, so nothing had to notice. This
/// reader matches exactly, so it did. Normalising here rather than editing the
/// pins is deliberate: the pin should describe the file, not which tar wrote it.
fn normalize(name: &str) -> &str {
    name.strip_prefix("./").unwrap_or(name)
}

fn nul_terminated(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Tar stores numbers as NUL/space-terminated octal ASCII.
fn parse_octal(field: &[u8]) -> Option<u64> {
    let text = nul_terminated(field);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(trimmed, 8).ok()
}

/// Member data is padded out to a whole number of 512-byte blocks.
fn padded(size: u64) -> u64 {
    size.div_ceil(TAR_BLOCK as u64) * TAR_BLOCK as u64
}

// ---------------------------------------------------------------------------
// zip
// ---------------------------------------------------------------------------

const ZIP_EOCD_SIG: u32 = 0x0605_4b50;
const ZIP_CD_SIG: u32 = 0x0201_4b50;
const ZIP_LFH_SIG: u32 = 0x0403_4b50;
/// The largest trailing comment a zip may carry, plus the fixed EOCD record.
const ZIP_MAX_EOCD_SEARCH: u64 = 0xffff + 22;

/// One central-directory entry, reduced to what this reader needs.
struct ZipEntry {
    compression: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_offset: u64,
}

fn extract_from_zip(
    archive: &Path,
    mut file: File,
    member: &str,
    sink: &mut dyn Write,
    limits: &Limits,
) -> Result<u64, ArchiveError> {
    let entry = find_zip_entry(archive, &mut file, member)?;

    if entry.uncompressed_size > limits.max_member_bytes {
        return Err(ArchiveError::TooLarge {
            archive: archive.to_path_buf(),
            detail: format!(
                "member is {} bytes, over the {}-byte ceiling",
                entry.uncompressed_size, limits.max_member_bytes
            ),
        });
    }

    // The local header's own name and extra-field lengths, NOT the central
    // directory's: the extra field is routinely a different length in the two
    // places, and skipping by the central one lands inside the member's data.
    file.seek(SeekFrom::Start(entry.local_offset))?;
    let mut lfh = [0u8; 30];
    read_exact_or_format(archive, &mut file, &mut lfh, "local file header")?;
    if u32_le(&lfh[0..4]) != ZIP_LFH_SIG {
        return Err(ArchiveError::Format {
            archive: archive.to_path_buf(),
            detail: "local file header is malformed".to_string(),
        });
    }
    let name_len = u16_le(&lfh[26..28]) as u64;
    let extra_len = u16_le(&lfh[28..30]) as u64;
    file.seek(SeekFrom::Current((name_len + extra_len) as i64))?;

    let mut crc = flate2::Crc::new();
    let mut counting = CrcWriter {
        inner: sink,
        crc: &mut crc,
    };
    let written = match entry.compression {
        0 => {
            if entry.compressed_size != entry.uncompressed_size {
                return Err(ArchiveError::Format {
                    archive: archive.to_path_buf(),
                    detail: "stored member has mismatched sizes".to_string(),
                });
            }
            copy_exactly(archive, &mut file, &mut counting, entry.uncompressed_size)?
        }
        8 => {
            let limited = (&mut file).take(entry.compressed_size);
            let mut inflater = DeflateDecoder::new(limited);
            copy_exactly(
                archive,
                &mut inflater,
                &mut counting,
                entry.uncompressed_size,
            )?
        }
        other => {
            return Err(ArchiveError::Unsupported {
                archive: archive.to_path_buf(),
                detail: format!("compression method {other}"),
            });
        }
    };

    // gzip carries its own CRC-32 and `GzDecoder` checks it, so the tar path
    // needs no equivalent. A zip's checksum is per member and nothing verifies
    // it unless the reader does.
    if crc.sum() != entry.crc32 {
        return Err(ArchiveError::Format {
            archive: archive.to_path_buf(),
            detail: format!(
                "member \"{member}\" fails its CRC-32: expected {:08x}, got {:08x}",
                entry.crc32,
                crc.sum()
            ),
        });
    }
    Ok(written)
}

fn find_zip_entry(archive: &Path, file: &mut File, member: &str) -> Result<ZipEntry, ArchiveError> {
    let size = file.metadata()?.len();
    let window = ZIP_MAX_EOCD_SEARCH.min(size);
    file.seek(SeekFrom::End(-(window as i64)))?;
    let mut tail = vec![0u8; window as usize];
    read_exact_or_format(archive, file, &mut tail, "end of archive")?;

    // Scan backwards: a zip whose comment happens to contain the signature
    // would otherwise win over the real record.
    let eocd = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_le(&tail[i..i + 4]) == ZIP_EOCD_SIG)
        .ok_or_else(|| ArchiveError::Format {
            archive: archive.to_path_buf(),
            detail: "no end-of-central-directory record".to_string(),
        })?;
    let eocd = &tail[eocd..];

    if u16_le(&eocd[4..6]) != 0 || u16_le(&eocd[6..8]) != 0 {
        return Err(ArchiveError::Unsupported {
            archive: archive.to_path_buf(),
            detail: "archive split across disks".to_string(),
        });
    }
    let count = u16_le(&eocd[10..12]) as usize;
    let cd_size = u32_le(&eocd[12..16]) as u64;
    let cd_offset = u32_le(&eocd[16..20]) as u64;
    if count == 0xffff || cd_size == 0xffff_ffff || cd_offset == 0xffff_ffff {
        return Err(ArchiveError::Unsupported {
            archive: archive.to_path_buf(),
            detail: "zip64 extensions".to_string(),
        });
    }
    if cd_offset + cd_size > size {
        return Err(ArchiveError::Format {
            archive: archive.to_path_buf(),
            detail: "central directory runs past the end of the archive".to_string(),
        });
    }

    file.seek(SeekFrom::Start(cd_offset))?;
    let mut cd = vec![0u8; cd_size as usize];
    read_exact_or_format(archive, file, &mut cd, "central directory")?;

    let mut p = 0usize;
    for _ in 0..count {
        if p + 46 > cd.len() || u32_le(&cd[p..p + 4]) != ZIP_CD_SIG {
            return Err(ArchiveError::Format {
                archive: archive.to_path_buf(),
                detail: "central directory is malformed".to_string(),
            });
        }
        let flags = u16_le(&cd[p + 8..p + 10]);
        let compression = u16_le(&cd[p + 10..p + 12]);
        let crc32 = u32_le(&cd[p + 16..p + 20]);
        let compressed_size = u32_le(&cd[p + 20..p + 24]) as u64;
        let uncompressed_size = u32_le(&cd[p + 24..p + 28]) as u64;
        let name_len = u16_le(&cd[p + 28..p + 30]) as usize;
        let extra_len = u16_le(&cd[p + 30..p + 32]) as usize;
        let comment_len = u16_le(&cd[p + 32..p + 34]) as usize;
        let local_offset = u32_le(&cd[p + 42..p + 46]) as u64;
        let name_end = p + 46 + name_len;
        if name_end > cd.len() {
            return Err(ArchiveError::Format {
                archive: archive.to_path_buf(),
                detail: "central directory entry name runs past its end".to_string(),
            });
        }
        let name = String::from_utf8_lossy(&cd[p + 46..name_end]).into_owned();

        if normalize(&name) == normalize(member) {
            if flags & 0x1 != 0 {
                return Err(ArchiveError::Unsupported {
                    archive: archive.to_path_buf(),
                    detail: format!("member \"{member}\" is encrypted"),
                });
            }
            if compressed_size == 0xffff_ffff || uncompressed_size == 0xffff_ffff {
                return Err(ArchiveError::Unsupported {
                    archive: archive.to_path_buf(),
                    detail: "zip64 extensions".to_string(),
                });
            }
            return Ok(ZipEntry {
                compression,
                crc32,
                compressed_size,
                uncompressed_size,
                local_offset,
            });
        }
        p = name_end + extra_len + comment_len;
    }

    Err(ArchiveError::MemberMissing {
        archive: archive.to_path_buf(),
        member: member.to_string(),
    })
}

// ---------------------------------------------------------------------------
// shared plumbing
// ---------------------------------------------------------------------------

/// A `Write` that also feeds a CRC-32.
struct CrcWriter<'a> {
    inner: &'a mut dyn Write,
    crc: &'a mut flate2::Crc,
}

impl Write for CrcWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Copy exactly `size` bytes, treating a short read as a truncated archive.
///
/// `io::copy` on a `take(size)` would return quietly on a short stream, which
/// is how a truncated archive becomes a silently truncated install.
fn copy_exactly(
    archive: &Path,
    reader: &mut dyn Read,
    sink: &mut dyn Write,
    size: u64,
) -> Result<u64, ArchiveError> {
    let mut remaining = size;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let want = buf.len().min(remaining as usize);
        let n = read_full(reader, &mut buf[..want])?;
        if n == 0 {
            return Err(ArchiveError::Format {
                archive: archive.to_path_buf(),
                detail: format!(
                    "archive ends inside a member's data: {remaining} of {size} bytes missing"
                ),
            });
        }
        sink.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(size)
}

fn skip(reader: &mut impl Read, mut bytes: u64) -> io::Result<()> {
    let mut buf = [0u8; 64 * 1024];
    while bytes > 0 {
        let want = buf.len().min(bytes as usize);
        let n = read_full(reader, &mut buf[..want])?;
        if n == 0 {
            return Ok(());
        }
        bytes -= n as u64;
    }
    Ok(())
}

/// `read` until the buffer is full or the stream ends — a decompressor returns
/// short reads at its own internal boundaries, which is not end-of-stream.
fn read_full(reader: &mut (impl Read + ?Sized), buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

fn read_exact_or_format(
    archive: &Path,
    reader: &mut impl Read,
    buf: &mut [u8],
    what: &str,
) -> Result<(), ArchiveError> {
    let n = read_full(reader, buf)?;
    if n < buf.len() {
        return Err(ArchiveError::Format {
            archive: archive.to_path_buf(),
            detail: format!("archive is too short to hold its {what}"),
        });
    }
    Ok(())
}

fn u16_le(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}
