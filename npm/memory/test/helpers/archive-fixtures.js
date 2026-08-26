"use strict";

// Archives for `archive-extract.test.js`, from two sources on purpose.
//
// GOLDENS are bytes from real, independent archivers — `bsdtar 3.5.3
// (libarchive 3.7.4)` and `Info-ZIP 3.0` — so the parser is checked against
// somebody else's idea of the format and not only against the writer below.
// They are base64 string constants rather than files because this repository
// tracks no binary file at all (`git ls-files` finds none), and two hundred
// bytes is a poor reason to make it track its first.
//
// The BUILDERS exist because the interesting fixtures cannot be produced by a
// real archiver at all: GNU tar strips a leading `/` by design, so "a member
// with an absolute path" is unbuildable with it, and the same goes for a
// deliberately wrong CRC or a zip64 sentinel. The suite round-trips a built
// archive through the parser before using any adversarial one, which is what
// makes those mean something.

const zlib = require("node:zlib");

// The one file inside every golden. Made compressible so Info-ZIP would pick
// deflate for one of them; `-0` forced stored for the other.
const GOLDEN_CONTENT =
  "#!/bin/sh\n" +
  "# stand-in for the real local-rag executable, T22-07 golden fixture\n" +
  "echo local-rag golden fixture line 0\n" +
  "echo local-rag golden fixture line 1\n" +
  "echo local-rag golden fixture line 2\n" +
  "echo local-rag golden fixture line 3\n" +
  "echo local-rag golden fixture line 4\n" +
  "echo local-rag golden fixture line 5\n";

const GOLDEN_NAME = "local-rag";

// Produced in a directory holding only `local-rag` (mode 755, mtime pinned with
// `touch -t 202601010000.00`, so the bytes are reproducible and carry nobody's
// uid or user name):
//
//   COPYFILE_DISABLE=1 tar --no-xattrs --uid 0 --uname root --gid 0 \
//     --gname wheel -czf g-plain.tar.gz -C src local-rag
//
// A plain ustar header, one member — the shape `cargo-dist`'s `tar` crate is
// expected to write.
const GOLDEN_TAR_GZ_PLAIN =
  "H4sIAFooj2oAA+2SwQqDMAyGPfsUGV4nprXFF9kLVM20UFqolfn46w4bmwfpYTIGfpcc/i+QtDGu" +
  "U6b0asj2AxEbKeFRI+uKKKQAJhmXdS1EDJDxRogMcMeZXsxTUD6O4p0LW95tJDIb+Xq5P6E4Va22" +
  "1TTmBcSXsH2pLVydhzASeFIGzPNCgBbq5qBaQ2e4cF5iA4MzPcUGvYTZU07d6N4aPlMw2hJgisRS" +
  "JJ4i1SmSSJFk/uu/Ojg4OPgmd86Jrp0ACAAA";

// The same command without `--no-xattrs`. libarchive then writes a pax extended
// header ahead of the member, because on macOS every file carries a
// `com.apple.provenance` xattr that cannot be removed. Kept as a golden
// precisely because it is the shape a strict "regular files only" reader would
// have rejected — and the shape a Mac produces by default.
const GOLDEN_TAR_GZ_PAX =
  "H4sIADgoj2oAA+2UsU7DMBCGA2OegeFQV5LYiU3EwBAQopE6lIKQEJOTHk0kE0euA4En4HV4DEae" +
  "g1dgICwFqqpkaIWAfMsN9//S3cm/h6Luoxij9qRKhXS0mFirhhAScg7vtWG+EkK5D5RTnwcBY02D" +
  "UBZSYkG98kkWUE2N0M0oWimzTHebIcol/fnlfgk8hEF8EI0O+/H5kVsLY7SbqmtXlKVEt9TqBgtR" +
  "pLgfncRRf3g/ukuO4106tdkenDamwcUy08amtfX6/PT48HJp//SmHYtYX+o/+C7/jLO5/PshYxaQ" +
  "Nc4045/nv7ftJXnhTTO7B80lirGTF3ClNJgMQaOQMHshgDWmlRGJxB04832HhDBRcoyNIa9NpdHG" +
  "NFOfDF+7IPMCgbQR0TYiv40oaCNibUS8+8E6Ojr+FG9ELHTzAAwAAA==";

// `cd src && zip -q -X -9 ../g-deflate.zip local-rag` — method 8, the path a
// real 30 MB executable takes.
const GOLDEN_ZIP_DEFLATE =
  "UEsDBBQAAgAIAAAAIVzsc3BQagAAACwBAAAJAAAAbG9jYWwtcmFnjcsxDsIwEETR3qcYlBYrwYC4" +
  "CBfYOENsaWVLjiPl+KGEbuv//nAZ51zGLbkBW5ey+FzwqQ09EY2i0BpFfZMVPBj3LrPyincIfnph" +
  "rbrwO+Sj742OMdWf4b9CcyEmC7pZULCguwU9LOjpTlBLAQIeAxQAAgAIAAAAIVzsc3BQagAAACwB" +
  "AAAJAAAAAAAAAAEAAADtgQAAAABsb2NhbC1yYWdQSwUGAAAAAAEAAQA3AAAAkQAAAAAA";

// `zip -q -X -0` — method 0. Not a curiosity: Info-ZIP picks stored on its own
// for any file deflate does not shrink, and a stored member read from the wrong
// offset is the one case that yields plausible bytes instead of an inflate
// error, which is what the CRC check is for.
const GOLDEN_ZIP_STORED =
  "UEsDBAoAAAAAAAAAIVzsc3BQLAEAACwBAAAJAAAAbG9jYWwtcmFnIyEvYmluL3NoCiMgc3RhbmQt" +
  "aW4gZm9yIHRoZSByZWFsIGxvY2FsLXJhZyBleGVjdXRhYmxlLCBUMjItMDcgZ29sZGVuIGZpeHR1" +
  "cmUKZWNobyBsb2NhbC1yYWcgZ29sZGVuIGZpeHR1cmUgbGluZSAwCmVjaG8gbG9jYWwtcmFnIGdv" +
  "bGRlbiBmaXh0dXJlIGxpbmUgMQplY2hvIGxvY2FsLXJhZyBnb2xkZW4gZml4dHVyZSBsaW5lIDIK" +
  "ZWNobyBsb2NhbC1yYWcgZ29sZGVuIGZpeHR1cmUgbGluZSAzCmVjaG8gbG9jYWwtcmFnIGdvbGRl" +
  "biBmaXh0dXJlIGxpbmUgNAplY2hvIGxvY2FsLXJhZyBnb2xkZW4gZml4dHVyZSBsaW5lIDUKUEsB" +
  "Ah4DCgAAAAAAAAAhXOxzcFAsAQAALAEAAAkAAAAAAAAAAAAAAO2BAAAAAGxvY2FsLXJhZ1BLBQYA" +
  "AAAAAQABADcAAABTAQAAAAA=";

/** @param {string} b64 @returns {Buffer} */
function golden(b64) {
  return Buffer.from(b64, "base64");
}

// ---------------------------------------------------------------------------
// tar
// ---------------------------------------------------------------------------

const TAR_BLOCK = 512;

function writeField(block, offset, value, length) {
  Buffer.from(value, "binary").copy(block, offset, 0, Math.min(value.length, length));
}

function octalField(value, digits) {
  return value.toString(8).padStart(digits, "0") + "\0";
}

/**
 * @param {{name: string, data?: Buffer|string, mode?: number, typeflag?: string,
 *   declaredSize?: number}} m
 */
function tarHeader(m) {
  const data = Buffer.from(m.data === undefined ? "" : m.data);
  const size = m.declaredSize === undefined ? data.length : m.declaredSize;
  const block = Buffer.alloc(TAR_BLOCK, 0);
  writeField(block, 0, m.name, 100);
  writeField(block, 100, octalField(m.mode === undefined ? 0o755 : m.mode, 7), 8);
  writeField(block, 108, octalField(0, 7), 8);
  writeField(block, 116, octalField(0, 7), 8);
  writeField(block, 124, octalField(size, 11), 12);
  writeField(block, 136, octalField(0, 11), 12);
  writeField(block, 156, m.typeflag === undefined ? "0" : m.typeflag, 1);
  writeField(block, 257, "ustar\0", 6);
  writeField(block, 263, "00", 2);
  writeField(block, 265, "root", 32);
  writeField(block, 297, "wheel", 32);

  // The checksum is computed with its own field read as spaces.
  block.fill(0x20, 148, 156);
  let sum = 0;
  for (let i = 0; i < TAR_BLOCK; i += 1) sum += block[i];
  writeField(block, 148, `${sum.toString(8).padStart(6, "0")}\0 `, 8);
  return { block, data };
}

/**
 * @param {Array<object>} members see `tarHeader`
 * @param {{trailer?: boolean, gzip?: boolean, corruptChecksumOf?: number}} [opts]
 * @returns {Buffer} a `.tar.gz` unless `gzip: false`
 */
function buildTar(members, opts = {}) {
  const parts = [];
  members.forEach((m, i) => {
    const { block, data } = tarHeader(m);
    if (opts.corruptChecksumOf === i) block[149] = block[149] === 0x30 ? 0x31 : 0x30;
    parts.push(block);
    if (data.length > 0) {
      const padded = Buffer.alloc(Math.ceil(data.length / TAR_BLOCK) * TAR_BLOCK, 0);
      data.copy(padded);
      parts.push(padded);
    }
  });
  if (opts.trailer !== false) parts.push(Buffer.alloc(TAR_BLOCK * 2, 0));
  const tar = Buffer.concat(parts);
  return opts.gzip === false ? tar : zlib.gzipSync(tar);
}

// ---------------------------------------------------------------------------
// zip
// ---------------------------------------------------------------------------

/**
 * @param {Array<{name: string, data?: Buffer|string, method?: 0|8, crc?: number,
 *   mode?: number, madeBy?: number, localExtra?: number,
 *   declaredSize?: number}>} entries
 * @param {{zip64?: boolean, entriesTotal?: number, comment?: string}} [opts]
 * @returns {Buffer}
 */
function buildZip(entries, opts = {}) {
  const local = [];
  const central = [];
  let offset = 0;

  for (const e of entries) {
    const data = Buffer.from(e.data === undefined ? "" : e.data);
    const method = e.method === undefined ? 0 : e.method;
    const payload = method === 8 ? zlib.deflateRawSync(data) : data;
    const crc = e.crc === undefined ? zlib.crc32(data) : e.crc;
    const name = Buffer.from(e.name, "utf8");
    // Extra bytes in the local header only — a real and common asymmetry, and
    // the one that puts a naive reader on the wrong offset.
    const localExtra = Buffer.alloc(e.localExtra === undefined ? 0 : e.localExtra, 0);
    // What the directory *claims* the member weighs, which a lying archive may
    // set to anything at all.
    const usize = e.declaredSize === undefined ? data.length : e.declaredSize;

    const lfh = Buffer.alloc(30);
    lfh.writeUInt32LE(0x04034b50, 0);
    lfh.writeUInt16LE(20, 4);
    lfh.writeUInt16LE(method, 8);
    lfh.writeUInt32LE(crc >>> 0, 14);
    lfh.writeUInt32LE(payload.length, 18);
    lfh.writeUInt32LE(usize, 22);
    lfh.writeUInt16LE(name.length, 26);
    lfh.writeUInt16LE(localExtra.length, 28);
    local.push(lfh, name, localExtra, payload);

    const cd = Buffer.alloc(46);
    cd.writeUInt32LE(0x02014b50, 0);
    cd.writeUInt16LE(e.madeBy === undefined ? 0x031e : e.madeBy, 4);
    cd.writeUInt16LE(20, 6);
    cd.writeUInt16LE(method, 10);
    cd.writeUInt32LE(crc >>> 0, 16);
    cd.writeUInt32LE(payload.length, 20);
    cd.writeUInt32LE(usize, 24);
    cd.writeUInt16LE(name.length, 28);
    cd.writeUInt32LE(((e.mode === undefined ? 0o100755 : e.mode) << 16) >>> 0, 38);
    cd.writeUInt32LE(offset, 42);
    central.push(cd, name);

    offset += lfh.length + name.length + localExtra.length + payload.length;
  }

  const localBytes = Buffer.concat(local);
  const centralBytes = Buffer.concat(central);
  const comment = Buffer.from(opts.comment === undefined ? "" : opts.comment, "utf8");
  const count = opts.entriesTotal === undefined ? entries.length : opts.entriesTotal;

  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(opts.zip64 ? 0xffff : count, 8);
  eocd.writeUInt16LE(opts.zip64 ? 0xffff : count, 10);
  eocd.writeUInt32LE(centralBytes.length, 12);
  eocd.writeUInt32LE(localBytes.length, 16);
  eocd.writeUInt16LE(comment.length, 20);

  return Buffer.concat([localBytes, centralBytes, eocd, comment]);
}

module.exports = {
  GOLDEN_CONTENT,
  GOLDEN_NAME,
  GOLDEN_TAR_GZ_PLAIN,
  GOLDEN_TAR_GZ_PAX,
  GOLDEN_ZIP_DEFLATE,
  GOLDEN_ZIP_STORED,
  golden,
  buildTar,
  buildZip,
};
