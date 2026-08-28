"use strict";

// The only module in this package — and in this repository — that reads an
// archive.
//
// `http.js` puts the release asset on disk and reports its sha256; `install.js`
// (T22-08) compares that against the published sidecar and decides what to
// trust; this module opens the archive it was handed and takes exactly one
// member out of it. Nothing here talks to the network, and nothing here is
// allowed to matter before the digest has been checked.
//
// WHY THIS EXISTS AT ALL, instead of shelling out to `tar`
//
// `cargo-dist` used to publish `.tar.xz`. `node:zlib` has gzip and inflate and
// will never have xz, so an installer that must work on a machine nobody has
// ever seen cannot unpack that with built-ins. Shelling out to the system
// archiver was considered and rejected in ADR-0013 on this project's own
// precedent: `crates/xtask/src/dist_ort.rs` used to shell out to `tar`, and its
// own module documentation argued that this was fine for a manually invoked
// development tool and not for something that must work offline in production —
// on minimal Linux images GNU tar delegates to an external `xz` binary that is
// not installed. (T22-15 finished that argument on the Rust side too: the
// runtime installer needed the same guarantee, so `crates/models/src/archive.rs`
// now reads both containers in-process and `dist_ort` uses it. The precedent
// stands; the `tar` call it pointed at is gone.) So T22-07 moved the producer instead: `dist-workspace.toml`
// now sets `unix-archive = ".tar.gz"` and `windows-archive = ".zip"`, the two
// formats Node can read unaided, and this module is the reader.
//
// WHAT THIS MODULE DELIBERATELY DOES NOT DO
//
// It does not verify the archive's sha256. That comparison is `install.js`'s,
// against the `.sha256` sidecar from the same release — the same split as
// `http.js`, which computes a digest and refuses to be the one that trusts it.
//
// It does not rename, fsync, chmod, or clean up after itself. It writes the
// member to a path the caller chose and returns; the atomic dance belongs to
// `install.js`, mirroring `crates/models/src/install.rs`.
//
// It does not shell out, and `archive-extract.test.js` asserts that by reading
// this file — the same shape as `release-urls.test.js`'s proof that `release.js`
// reaches no socket.
//
// Path traversal is not the property the member-name rules buy. The member is
// written to `destPath`, which the caller chose; an archive's own path is never
// joined onto anything, so zip-slip is structurally impossible here rather than
// defended against. The rules below reject an absolute path, a `..` segment and
// a foreign name because such an archive is *not the one we published*, and a
// loud stop is a better answer to that than a silent extraction. They also keep
// holding if some later caller does start joining paths.
//
// STREAMING WAS CONSIDERED AND REJECTED. A zip's index lives in its tail, so a
// front-to-back stream would cover one of the two formats at roughly twice the
// code. The archive is a local file that was just downloaded and verified, and
// the peak here is about one and a half times the executable — some 45 MiB for
// a 30 MB binary, bounded below by the ceilings and above by nobody's budget.
//
// WHAT THE TESTS DO NOT PROVE. The golden fixtures come from `bsdtar` and
// Info-ZIP: real, independent implementations, but not the one that cuts the
// release (the `tar` and `zip` crates inside `cargo-dist`). T22-17 cuts the
// first tag in the new format and installs from it for real; that is where the
// producer is checked. Until then this parser is strict on purpose — an
// unexpected archive shape stops loudly instead of installing something else.

const zlib = require("node:zlib");
const fs = require("node:fs");

// Generous against the ~30 MB the biggest binary actually is, and small enough
// that a decompression bomb is refused rather than paged in. Both are checked
// before the allocation they bound, never after.
const MAX_ARCHIVE_BYTES = 128 * 1024 * 1024;
const MAX_MEMBER_BYTES = 128 * 1024 * 1024;
// A single-member tar is its member plus a few 512-byte blocks; the slack
// covers a pax header and a directory entry without loosening the member bound.
const TAR_METADATA_SLACK = 64 * 1024;

/**
 * Both ceilings are overridable for the same reason `http.js`'s timeouts are:
 * a test that had to build a 128 MiB fixture to reach one would be a worse test
 * than the bug it guards against.
 *
 * @param {{maxArchiveBytes?: number, maxMemberBytes?: number}} [opts]
 */
function limitsFrom(opts = {}) {
  const maxMemberBytes =
    opts.maxMemberBytes === undefined ? MAX_MEMBER_BYTES : opts.maxMemberBytes;
  return {
    maxArchiveBytes:
      opts.maxArchiveBytes === undefined ? MAX_ARCHIVE_BYTES : opts.maxArchiveBytes,
    maxMemberBytes,
    maxTarBytes: maxMemberBytes + TAR_METADATA_SLACK,
  };
}

const TAR_BLOCK = 512;

const ZIP_EOCD_SIG = 0x06054b50;
const ZIP_CD_SIG = 0x02014b50;
const ZIP_LFH_SIG = 0x04034b50;
// The largest a zip's trailing comment may be, plus the fixed EOCD record.
const ZIP_MAX_EOCD_SEARCH = 0xffff + 22;

/** Errors carry a kind so the caller can tell a bad archive from a big one. */
class ArchiveError extends Error {
  /** @param {"format"|"member"|"too-large"|"unsupported"|"integrity"} kind */
  constructor(kind, message, { archive } = {}) {
    super(message);
    this.name = "ArchiveError";
    this.kind = kind;
    this.archive = archive;
  }
}

// ---------------------------------------------------------------------------
// Member names — the rules are the same for both formats, so they live once.
// ---------------------------------------------------------------------------

/**
 * @param {string} name the member's own path, as the archive records it
 * @param {string} expectedName e.g. "local-rag" or "local-rag.exe"
 * @param {string} archive
 * @returns {string} the basename, once it has been accepted
 */
function acceptMemberName(name, expectedName, archive) {
  // Windows producers write backslashes; compare one shape, not two.
  const p = name.replace(/\\/g, "/");
  if (p.startsWith("/") || p.startsWith("//") || /^[A-Za-z]:/.test(p)) {
    throw new ArchiveError("member", `member "${name}" has an absolute path`, { archive });
  }
  const segments = p.split("/");
  if (segments.some((s) => s === ".." || s === "." || s === "")) {
    throw new ArchiveError("member", `member "${name}" has a "." or ".." segment`, {
      archive,
    });
  }
  const base = segments[segments.length - 1];
  if (base !== expectedName) {
    throw new ArchiveError(
      "member",
      `archive holds "${name}", not the expected "${expectedName}"`,
      { archive },
    );
  }
  return base;
}

/** @param {Array<{name: string}>} files @param {string} archive */
function requireExactlyOne(files, expectedName, archive) {
  if (files.length === 0) {
    throw new ArchiveError("member", `archive holds no file named "${expectedName}"`, {
      archive,
    });
  }
  if (files.length > 1) {
    const named = files.map((f) => `"${f.name}"`).join(", ");
    throw new ArchiveError(
      "member",
      `archive holds ${files.length} files (${named}); exactly one was expected`,
      { archive },
    );
  }
  return files[0];
}

// ---------------------------------------------------------------------------
// tar
// ---------------------------------------------------------------------------

/** @param {Buffer} block @returns {boolean} */
function isZeroBlock(block) {
  for (let i = 0; i < block.length; i += 1) {
    if (block[i] !== 0) return false;
  }
  return true;
}

/** A NUL- or space-terminated octal field. */
function parseOctal(block, offset, length, archive) {
  const raw = block.subarray(offset, offset + length).toString("binary");
  const text = raw.replace(/\0/g, " ").trim();
  if (text === "") return 0;
  if (!/^[0-7]+$/.test(text)) {
    throw new ArchiveError("format", `tar header holds "${text}" where an octal was due`, {
      archive,
    });
  }
  return parseInt(text, 8);
}

/** A NUL-terminated string field. */
function parseString(block, offset, length) {
  const raw = block.subarray(offset, offset + length);
  const end = raw.indexOf(0);
  return raw.subarray(0, end === -1 ? raw.length : end).toString("utf8");
}

/**
 * The header checksum is the sum of all 512 bytes with the checksum field
 * itself read as spaces. It is the one cheap way to tell "not a tar at all"
 * from "a tar whose size field should be believed", which matters because that
 * size is what the reader then walks by.
 */
function verifyTarChecksum(block, archive) {
  const stored = parseOctal(block, 148, 8, archive);
  let unsigned = 0;
  let signed = 0;
  for (let i = 0; i < TAR_BLOCK; i += 1) {
    const b = i >= 148 && i < 156 ? 0x20 : block[i];
    unsigned += b;
    signed += b > 127 ? b - 256 : b;
  }
  // Historic writers disagreed on the sign of the bytes; accept either sum.
  if (stored !== unsigned && stored !== signed) {
    throw new ArchiveError("format", "tar header checksum does not match", { archive });
  }
}

/**
 * @returns {{name: string, data: Buffer, mode: number|null}}
 */
function readTar(tar, expectedName, archive, limits) {
  const files = [];
  let off = 0;
  while (off + TAR_BLOCK <= tar.length) {
    const header = tar.subarray(off, off + TAR_BLOCK);
    // A zero block ends the archive. Producers write two; one is decisive.
    if (isZeroBlock(header)) break;
    verifyTarChecksum(header, archive);

    const size = parseOctal(header, 124, 12, archive);
    if (size > limits.maxMemberBytes) {
      throw new ArchiveError(
        "too-large",
        `tar member declares ${size} bytes, over the ${limits.maxMemberBytes}-byte ceiling`,
        { archive },
      );
    }
    const dataOff = off + TAR_BLOCK;
    if (dataOff + size > tar.length) {
      throw new ArchiveError("format", "tar ends inside a member's data", { archive });
    }

    const typeflag = String.fromCharCode(header[156]);
    let name = parseString(header, 0, 100);
    const magic = header.subarray(257, 263).toString("binary");
    if (magic === "ustar\0") {
      const prefix = parseString(header, 345, 155);
      if (prefix !== "") name = `${prefix}/${name}`;
    }

    if (typeflag === "0" || typeflag === "\0") {
      // Pre-POSIX writers marked a directory by a trailing slash, not a type.
      if (name.endsWith("/")) {
        // a directory, nothing to take
      } else {
        files.push({
          name,
          data: tar.subarray(dataOff, dataOff + size),
          mode: parseOctal(header, 100, 8, archive) & 0o7777,
        });
      }
    } else if (typeflag === "5") {
      // A directory entry. `cargo-dist` writes members at the archive root, but
      // an archive that nests them is still ours as long as one file matches.
    } else if (typeflag === "x" || typeflag === "g") {
      // A pax extended header, skipped without being read. It carries metadata
      // (mtime, xattrs, ownership) about the entry that follows, never the
      // bytes we want, and the ustar name of that entry stays authoritative for
      // the short names this project publishes. Not hypothetical: libarchive
      // emits one for any file carrying `com.apple.provenance`, which on macOS
      // is every file — one of the golden fixtures is exactly that shape.
    } else if (typeflag === "L" || typeflag === "K") {
      // GNU long name/link. Here the ustar name IS truncated, so comparing it
      // against `expectedName` would be comparing the wrong string. Our names
      // are ~40 characters and can never need this.
      throw new ArchiveError("unsupported", "tar uses GNU long-name entries", { archive });
    } else {
      throw new ArchiveError(
        "member",
        `tar member "${name}" is not a regular file (typeflag "${typeflag}")`,
        { archive },
      );
    }

    off = dataOff + Math.ceil(size / TAR_BLOCK) * TAR_BLOCK;
  }

  const file = requireExactlyOne(files, expectedName, archive);
  acceptMemberName(file.name, expectedName, archive);
  return file;
}

/** @returns {Buffer} the tar inside a gzip stream */
function gunzip(buf, archive, limits) {
  try {
    return zlib.gunzipSync(buf, { maxOutputLength: limits.maxTarBytes });
  } catch (err) {
    if (err && err.code === "ERR_BUFFER_TOO_LARGE") {
      throw new ArchiveError(
        "too-large",
        `gzip stream expands past the ${limits.maxTarBytes}-byte ceiling`,
        { archive },
      );
    }
    // gunzipSync verifies the CRC-32 in the gzip trailer itself, so a corrupt
    // stream arrives here rather than as plausible bytes. That is why the zip
    // side below has to check a CRC by hand and this side does not.
    throw new ArchiveError("format", `not a readable gzip stream: ${err.message}`, {
      archive,
    });
  }
}

// ---------------------------------------------------------------------------
// zip
// ---------------------------------------------------------------------------

/**
 * The record is at the very end unless the zip carries a trailing comment, so
 * it is found by scanning backwards over the largest a comment may be.
 *
 * @returns {number} offset of the end-of-central-directory record
 */
function findEocd(buf, archive) {
  const from = Math.max(0, buf.length - ZIP_MAX_EOCD_SEARCH);
  for (let i = buf.length - 22; i >= from; i -= 1) {
    if (buf.readUInt32LE(i) === ZIP_EOCD_SIG) return i;
  }
  throw new ArchiveError("format", "zip has no end-of-central-directory record", {
    archive,
  });
}

/**
 * @returns {{name: string, data: Buffer, mode: number|null}}
 */
function readZip(buf, expectedName, archive, limits) {
  if (buf.length < 22) {
    throw new ArchiveError("format", "zip is too short to hold a directory", { archive });
  }
  const eocd = findEocd(buf, archive);
  const diskNumber = buf.readUInt16LE(eocd + 4);
  const cdDisk = buf.readUInt16LE(eocd + 6);
  const entriesHere = buf.readUInt16LE(eocd + 8);
  const entriesTotal = buf.readUInt16LE(eocd + 10);
  const cdSize = buf.readUInt32LE(eocd + 12);
  const cdOffset = buf.readUInt32LE(eocd + 16);

  if (entriesTotal === 0xffff || cdSize === 0xffffffff || cdOffset === 0xffffffff) {
    throw new ArchiveError("unsupported", "zip uses the zip64 extensions", { archive });
  }
  if (diskNumber !== 0 || cdDisk !== 0 || entriesHere !== entriesTotal) {
    throw new ArchiveError("unsupported", "zip is split across disks", { archive });
  }
  if (cdOffset + cdSize > buf.length) {
    throw new ArchiveError("format", "zip's central directory runs past its end", {
      archive,
    });
  }

  const files = [];
  let p = cdOffset;
  for (let i = 0; i < entriesTotal; i += 1) {
    if (p + 46 > cdOffset + cdSize || buf.readUInt32LE(p) !== ZIP_CD_SIG) {
      throw new ArchiveError("format", "zip's central directory is malformed", { archive });
    }
    const madeBy = buf.readUInt16LE(p + 4);
    const flags = buf.readUInt16LE(p + 8);
    const method = buf.readUInt16LE(p + 10);
    const crc = buf.readUInt32LE(p + 16);
    const compressedSize = buf.readUInt32LE(p + 20);
    const uncompressedSize = buf.readUInt32LE(p + 24);
    const nameLength = buf.readUInt16LE(p + 28);
    const extraLength = buf.readUInt16LE(p + 30);
    const commentLength = buf.readUInt16LE(p + 32);
    const externalAttributes = buf.readUInt32LE(p + 38);
    const localOffset = buf.readUInt32LE(p + 42);
    const name = buf.subarray(p + 46, p + 46 + nameLength).toString("utf8");
    p += 46 + nameLength + extraLength + commentLength;

    // A directory entry, by the only portable marker there is.
    if (name.endsWith("/")) continue;

    if ((flags & 0x1) !== 0) {
      throw new ArchiveError("unsupported", `zip member "${name}" is encrypted`, { archive });
    }
    if (
      compressedSize === 0xffffffff ||
      uncompressedSize === 0xffffffff ||
      localOffset === 0xffffffff
    ) {
      throw new ArchiveError("unsupported", "zip uses the zip64 extensions", { archive });
    }
    if (uncompressedSize > limits.maxMemberBytes) {
      throw new ArchiveError(
        "too-large",
        `zip member declares ${uncompressedSize} bytes, over the ` +
          `${limits.maxMemberBytes}-byte ceiling`,
        { archive },
      );
    }
    files.push({
      name,
      method,
      crc,
      compressedSize,
      uncompressedSize,
      externalAttributes,
      madeBy,
      localOffset,
    });
  }

  const entry = requireExactlyOne(files, expectedName, archive);
  acceptMemberName(entry.name, expectedName, archive);

  // The local header is read only for where its data starts. Its size fields
  // may be zeroed in favour of a trailing data descriptor, and its extra field
  // is routinely a different length from the central one — the classic way to
  // read a stored member from the wrong offset — so every number that matters
  // comes from the central directory above.
  if (
    entry.localOffset + 30 > buf.length ||
    buf.readUInt32LE(entry.localOffset) !== ZIP_LFH_SIG
  ) {
    throw new ArchiveError("format", "zip's local file header is malformed", { archive });
  }
  const dataStart =
    entry.localOffset +
    30 +
    buf.readUInt16LE(entry.localOffset + 26) +
    buf.readUInt16LE(entry.localOffset + 28);
  if (dataStart + entry.compressedSize > buf.length) {
    throw new ArchiveError("format", "zip ends inside a member's data", { archive });
  }
  const raw = buf.subarray(dataStart, dataStart + entry.compressedSize);

  let data;
  if (entry.method === 0) {
    if (entry.compressedSize !== entry.uncompressedSize) {
      throw new ArchiveError("format", "stored zip member has mismatched sizes", { archive });
    }
    data = raw;
  } else if (entry.method === 8) {
    try {
      data = zlib.inflateRawSync(raw, { maxOutputLength: limits.maxMemberBytes });
    } catch (err) {
      if (err && err.code === "ERR_BUFFER_TOO_LARGE") {
        throw new ArchiveError(
          "too-large",
          `zip member expands past the ${limits.maxMemberBytes}-byte ceiling`,
          { archive },
        );
      }
      throw new ArchiveError("format", `zip member does not inflate: ${err.message}`, {
        archive,
      });
    }
  } else {
    throw new ArchiveError(
      "unsupported",
      `zip member uses compression method ${entry.method}; only stored and deflate are read`,
      { archive },
    );
  }

  if (data.length !== entry.uncompressedSize) {
    throw new ArchiveError(
      "integrity",
      `zip member is ${data.length} bytes, not the ${entry.uncompressedSize} its directory claims`,
      { archive },
    );
  }
  // Raw deflate carries no checksum of its own — unlike a gzip stream, which is
  // why this check exists on this side only. It is what catches a member read
  // from the wrong offset: inflate would usually throw, but a *stored* member
  // read from the wrong offset yields plausible bytes and would otherwise be
  // installed as if it were the executable.
  const actualCrc = zlib.crc32(data);
  if (actualCrc !== entry.crc) {
    throw new ArchiveError(
      "integrity",
      `zip member's CRC-32 is ${actualCrc.toString(16)}, not the ` +
        `${entry.crc.toString(16)} its directory claims`,
      { archive },
    );
  }

  // The high byte of "version made by" is the source platform; 3 is Unix, and
  // only there do the external attributes carry a mode. A zip written on
  // Windows has DOS attributes instead, so the mode is genuinely unknown.
  const mode = entry.madeBy >> 8 === 3 ? (entry.externalAttributes >>> 16) & 0o7777 : null;
  return { name: entry.name, data, mode };
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/**
 * Which reader to use, decided by the bytes and never by the file name.
 *
 * The name comes from `release.js`'s `assetName`, and a name that disagrees
 * with the producer is precisely the skew T22-07 closed; dispatching on the
 * name would put that same disagreement back, one layer down.
 */
function sniffFormat(buf) {
  if (buf.length >= 2 && buf[0] === 0x1f && buf[1] === 0x8b) return "tar.gz";
  // Any of the zip signatures, not just a local file header: an empty or
  // spanned zip starts with a different one, and reaching the zip reader with
  // it produces an accurate complaint instead of "this is not an archive".
  if (buf.length >= 4 && buf[0] === 0x50 && buf[1] === 0x4b) return "zip";
  if (buf.length >= 6 && buf.subarray(0, 6).toString("binary") === "\xfd7zXZ\0") return "xz";
  return null;
}

/**
 * Take the single member named `expectedName` out of `archivePath` and write it
 * to `destPath`.
 *
 * @param {string} archivePath a `.tar.gz` or `.zip` already on disk
 * @param {string} expectedName `release.js`'s `executableName(binary, platform)`
 * @param {string} destPath written only once the member has been fully accepted
 * @param {{maxArchiveBytes?: number, maxMemberBytes?: number}} [opts]
 * @returns {{bytesWritten: number, memberName: string, mode: number|null}}
 *   `mode` is the archived permission bits, or null when the archive does not
 *   record any — informational: `install.js` sets the mode it wants.
 */
function extractSingleMember(archivePath, expectedName, destPath, opts = {}) {
  const limits = limitsFrom(opts);
  const stat = fs.statSync(archivePath);
  if (!stat.isFile()) {
    throw new ArchiveError("format", `${archivePath} is not a file`, { archive: archivePath });
  }
  if (stat.size > limits.maxArchiveBytes) {
    throw new ArchiveError(
      "too-large",
      `archive is ${stat.size} bytes, over the ${limits.maxArchiveBytes}-byte ceiling`,
      { archive: archivePath },
    );
  }

  const buf = fs.readFileSync(archivePath);
  const format = sniffFormat(buf);
  let member;
  if (format === "tar.gz") {
    member = readTar(gunzip(buf, archivePath, limits), expectedName, archivePath, limits);
  } else if (format === "zip") {
    member = readZip(buf, expectedName, archivePath, limits);
  } else if (format === "xz") {
    throw new ArchiveError(
      "unsupported",
      "archive is xz-compressed; releases moved to .tar.gz in T22-07 because Node cannot read xz",
      { archive: archivePath },
    );
  } else {
    throw new ArchiveError("format", "archive is neither gzip nor zip", {
      archive: archivePath,
    });
  }

  // Last, so that every rejection above leaves `destPath` untouched.
  fs.writeFileSync(destPath, member.data);
  return { bytesWritten: member.data.length, memberName: member.name, mode: member.mode };
}

module.exports = {
  ArchiveError,
  MAX_ARCHIVE_BYTES,
  MAX_MEMBER_BYTES,
  extractSingleMember,
  sniffFormat,
};
