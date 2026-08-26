"use strict";

// Taking one member out of a release archive (T22-07). Two kinds of fixture,
// and the difference matters: the goldens are bytes from `bsdtar` and Info-ZIP,
// so they check the parser against somebody else's format implementation, while
// the built ones exist because the adversarial shapes cannot be produced by a
// real archiver at all — GNU tar strips a leading `/` by design.
//
// What none of this proves is the producer. The release is cut by the `tar` and
// `zip` crates inside `cargo-dist`, and T22-17 is the card that installs from a
// real tag in the new format. Until then the parser is strict on purpose.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const zlib = require("node:zlib");

const { extractSingleMember, ArchiveError } = require("../src/archive.js");
const { mkTmpRoot } = require("./helpers/tmp.js");
const {
  GOLDEN_CONTENT,
  GOLDEN_NAME,
  GOLDEN_TAR_GZ_PLAIN,
  GOLDEN_TAR_GZ_PAX,
  GOLDEN_ZIP_DEFLATE,
  GOLDEN_ZIP_STORED,
  golden,
  buildTar,
  buildZip,
} = require("./helpers/archive-fixtures.js");

const PAYLOAD = Buffer.from("the bytes that stand in for a native executable\n");

/** Lay `archive` down in a fresh directory and return it with a dest path. */
function scratch(prefix, archive) {
  const root = mkTmpRoot(`lr-archive-${prefix}-`);
  const archivePath = path.join(root, "asset");
  fs.writeFileSync(archivePath, archive);
  return { root, archivePath, dest: path.join(root, "out") };
}

function isKind(kind, pattern) {
  return (err) =>
    err instanceof ArchiveError && err.kind === kind && pattern.test(err.message);
}

// ---------------------------------------------------------------------------
// Real archivers
// ---------------------------------------------------------------------------

test("a golden .tar.gz from bsdtar gives the member back byte for byte", () => {
  const { root, archivePath, dest } = scratch("tar-plain", golden(GOLDEN_TAR_GZ_PLAIN));
  const out = extractSingleMember(archivePath, GOLDEN_NAME, dest);
  assert.equal(fs.readFileSync(dest, "utf8"), GOLDEN_CONTENT);
  assert.equal(out.bytesWritten, Buffer.byteLength(GOLDEN_CONTENT));
  assert.equal(out.memberName, GOLDEN_NAME);
  assert.equal(out.mode, 0o755);
  fs.rmSync(root, { recursive: true, force: true });
});

test("a golden .tar.gz carrying a pax header is read, not refused", () => {
  // libarchive writes one for any file with an xattr, which on macOS is every
  // file. A reader that admitted only regular entries would reject this.
  const { root, archivePath, dest } = scratch("tar-pax", golden(GOLDEN_TAR_GZ_PAX));
  extractSingleMember(archivePath, GOLDEN_NAME, dest);
  assert.equal(fs.readFileSync(dest, "utf8"), GOLDEN_CONTENT);
  fs.rmSync(root, { recursive: true, force: true });
});

test("a golden deflated .zip from Info-ZIP gives the member back byte for byte", () => {
  const { root, archivePath, dest } = scratch("zip-deflate", golden(GOLDEN_ZIP_DEFLATE));
  const out = extractSingleMember(archivePath, GOLDEN_NAME, dest);
  assert.equal(fs.readFileSync(dest, "utf8"), GOLDEN_CONTENT);
  assert.equal(out.mode, 0o755);
  fs.rmSync(root, { recursive: true, force: true });
});

test("a golden stored .zip from Info-ZIP gives the member back byte for byte", () => {
  const { root, archivePath, dest } = scratch("zip-stored", golden(GOLDEN_ZIP_STORED));
  extractSingleMember(archivePath, GOLDEN_NAME, dest);
  assert.equal(fs.readFileSync(dest, "utf8"), GOLDEN_CONTENT);
  fs.rmSync(root, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// The builders, before anything adversarial leans on them
// ---------------------------------------------------------------------------

test("a built .tar.gz and a built .zip both round-trip through the reader", () => {
  for (const [label, archive] of [
    ["tar", buildTar([{ name: "local-rag", data: PAYLOAD }])],
    ["zip-stored", buildZip([{ name: "local-rag", data: PAYLOAD }])],
    ["zip-deflate", buildZip([{ name: "local-rag", data: PAYLOAD, method: 8 }])],
  ]) {
    const { root, archivePath, dest } = scratch(label, archive);
    const out = extractSingleMember(archivePath, "local-rag", dest);
    assert.deepEqual(fs.readFileSync(dest), PAYLOAD, label);
    assert.equal(out.bytesWritten, PAYLOAD.length, label);
    fs.rmSync(root, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Which reader runs
// ---------------------------------------------------------------------------

test("the format comes from the bytes, never from the file name", () => {
  // The name is built by `release.js`'s `assetName`, and a name that disagrees
  // with the producer is the exact skew this card closed; dispatching on it
  // would put that disagreement back one layer down.
  const root = mkTmpRoot("lr-archive-sniff-");
  const misnamed = path.join(root, "local-rag-x86_64-pc-windows-msvc.zip");
  fs.writeFileSync(misnamed, buildTar([{ name: "local-rag", data: PAYLOAD }]));
  const dest = path.join(root, "out");
  extractSingleMember(misnamed, "local-rag", dest);
  assert.deepEqual(fs.readFileSync(dest), PAYLOAD);
  fs.rmSync(root, { recursive: true, force: true });
});

test("an xz archive says so, rather than being reported as unreadable rubbish", () => {
  // A tag cut before T22-07 still carries `.tar.xz`, and pointing the installer
  // at one should explain itself in a line.
  const xz = Buffer.concat([Buffer.from("\xfd7zXZ\0", "binary"), Buffer.alloc(64, 7)]);
  const { root, archivePath, dest } = scratch("xz", xz);
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("unsupported", /xz-compressed/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("bytes that are neither gzip nor zip are a typed format error", () => {
  const { root, archivePath, dest } = scratch("junk", Buffer.from("not an archive at all"));
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("format", /neither gzip nor zip/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// Exactly one member
// ---------------------------------------------------------------------------

test("two files in a tar are refused, and both are named", () => {
  const { root, archivePath, dest } = scratch(
    "tar-two",
    buildTar([
      { name: "._local-rag", data: "AppleDouble metadata" },
      { name: "local-rag", data: PAYLOAD },
    ]),
  );
  assert.throws(() => extractSingleMember(archivePath, "local-rag", dest), (err) => {
    assert.ok(err instanceof ArchiveError && err.kind === "member");
    assert.match(err.message, /2 files/);
    assert.match(err.message, /"\._local-rag"/);
    assert.match(err.message, /"local-rag"/);
    return true;
  });
  fs.rmSync(root, { recursive: true, force: true });
});

test("two entries in a zip are refused", () => {
  const { root, archivePath, dest } = scratch(
    "zip-two",
    buildZip([
      { name: "local-rag", data: PAYLOAD },
      { name: "README.md", data: "auto-includes would have put this here" },
    ]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("member", /2 files/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a directory entry alongside the file is not a second member", () => {
  for (const [label, archive] of [
    [
      "tar",
      buildTar([
        { name: "local-rag-1.0/", typeflag: "5" },
        { name: "local-rag-1.0/local-rag", data: PAYLOAD },
      ]),
    ],
    [
      "zip",
      buildZip([
        { name: "local-rag-1.0/", data: "" },
        { name: "local-rag-1.0/local-rag", data: PAYLOAD },
      ]),
    ],
  ]) {
    const { root, archivePath, dest } = scratch(label, archive);
    const out = extractSingleMember(archivePath, "local-rag", dest);
    assert.deepEqual(fs.readFileSync(dest), PAYLOAD, label);
    assert.equal(out.memberName, "local-rag-1.0/local-rag", label);
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("an archive holding no file of that name says so", () => {
  const { root, archivePath, dest } = scratch("empty", buildTar([]));
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("member", /no file named "local-rag"/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// Member names
// ---------------------------------------------------------------------------

test("a member with an absolute path is refused, in either format", () => {
  for (const [label, archive] of [
    ["tar", buildTar([{ name: "/etc/local-rag", data: PAYLOAD }])],
    ["zip", buildZip([{ name: "/etc/local-rag", data: PAYLOAD }])],
  ]) {
    const { root, archivePath, dest } = scratch(label, archive);
    assert.throws(
      () => extractSingleMember(archivePath, "local-rag", dest),
      isKind("member", /absolute path/),
      label,
    );
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a member with a Windows drive letter is refused", () => {
  const { root, archivePath, dest } = scratch(
    "drive",
    buildZip([{ name: "C:\\Windows\\local-rag", data: PAYLOAD }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("member", /absolute path/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a member with a `..` segment is refused, in either format", () => {
  for (const [label, archive] of [
    ["tar", buildTar([{ name: "../../../etc/local-rag", data: PAYLOAD }])],
    ["zip", buildZip([{ name: "../../../etc/local-rag", data: PAYLOAD }])],
  ]) {
    const { root, archivePath, dest } = scratch(label, archive);
    assert.throws(
      () => extractSingleMember(archivePath, "local-rag", dest),
      isKind("member", /"\." or "\.\." segment/),
      label,
    );
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("a member under a different name is refused, and both names are in the message", () => {
  const { root, archivePath, dest } = scratch(
    "wrong-name",
    buildTar([{ name: "local-rag-proxy", data: PAYLOAD }]),
  );
  assert.throws(() => extractSingleMember(archivePath, "local-rag", dest), (err) => {
    assert.ok(err instanceof ArchiveError && err.kind === "member");
    assert.match(err.message, /"local-rag-proxy"/);
    assert.match(err.message, /expected "local-rag"/);
    return true;
  });
  fs.rmSync(root, { recursive: true, force: true });
});

test("a symlink member is refused, and its typeflag is named", () => {
  const { root, archivePath, dest } = scratch(
    "symlink",
    buildTar([{ name: "local-rag", data: "", typeflag: "2" }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("member", /typeflag "2"/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a GNU long-name entry is refused as unsupported, not misread", () => {
  // There the ustar name is truncated, so comparing it against the expected one
  // would be comparing the wrong string.
  const { root, archivePath, dest } = scratch(
    "longname",
    buildTar([
      { name: "././@LongLink", data: "a".repeat(120), typeflag: "L" },
      { name: "local-rag", data: PAYLOAD },
    ]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("unsupported", /GNU long-name/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// Malformed and oversized
// ---------------------------------------------------------------------------

test("a corrupt tar header checksum is a format error, not a wild read", () => {
  const { root, archivePath, dest } = scratch(
    "badsum",
    buildTar([{ name: "local-rag", data: PAYLOAD }], { corruptChecksumOf: 0 }),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("format", /checksum does not match/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a tar that ends inside its member's data is a typed error, not a TypeError", () => {
  const { root, archivePath, dest } = scratch(
    "truncated",
    buildTar([{ name: "local-rag", data: PAYLOAD, declaredSize: 5000 }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("format", /ends inside a member's data/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a member declaring more than the ceiling is refused before it is read", () => {
  // The declared size is what is checked, so the fixture stays tiny: a real
  // 200 MiB fixture would be the bug this guards against.
  const { root, archivePath, dest } = scratch(
    "huge",
    buildTar([{ name: "local-rag", data: PAYLOAD, declaredSize: 200 * 1024 * 1024 }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("too-large", /over the \d+-byte ceiling/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("an archive larger than the ceiling is refused before a byte is read", () => {
  const { root, archivePath, dest } = scratch(
    "big-archive",
    buildTar([{ name: "local-rag", data: PAYLOAD }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest, { maxArchiveBytes: 16 }),
    isKind("too-large", /archive is \d+ bytes, over the 16-byte ceiling/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a gzip stream that expands past the ceiling is stopped mid-decompression", () => {
  // 256 KiB of zeroes in a few hundred bytes: the shape of a decompression
  // bomb, in miniature. The ceilings are overridable so the fixture can stay
  // small — a real 128 MiB one would be the very thing being guarded against.
  const bomb = zlib.gzipSync(Buffer.alloc(256 * 1024, 0));
  const { root, archivePath, dest } = scratch("bomb", bomb);
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest, { maxMemberBytes: 1024 }),
    isKind("too-large", /gzip stream expands past/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a zip member declaring more than the ceiling is refused before it is inflated", () => {
  const { root, archivePath, dest } = scratch(
    "zip-huge",
    buildZip([{ name: "local-rag", data: PAYLOAD, method: 8 }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest, { maxMemberBytes: 4 }),
    isKind("too-large", /zip member declares \d+ bytes/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a zip directory that lies about its member's size is refused", () => {
  const { root, archivePath, dest } = scratch(
    "zip-lie",
    buildZip([{ name: "local-rag", data: PAYLOAD, method: 8, declaredSize: PAYLOAD.length + 9 }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("integrity", /its directory claims/),
  );
  assert.equal(fs.existsSync(dest), false);
  fs.rmSync(root, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// zip particulars
// ---------------------------------------------------------------------------

test("a zip member whose CRC-32 disagrees with its bytes is refused", () => {
  // The stored checksum is patched rather than the data, so inflate succeeds
  // and it is the check itself that has to fire.
  const { root, archivePath, dest } = scratch(
    "badcrc",
    buildZip([{ name: "local-rag", data: PAYLOAD, method: 8, crc: 0xdeadbeef }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("integrity", /CRC-32/),
  );
  assert.equal(fs.existsSync(dest), false);
  fs.rmSync(root, { recursive: true, force: true });
});

test("a longer extra field in the local header than in the central one is honoured", () => {
  // The classic zip-parser bug: the two lengths routinely differ, and taking
  // the central one puts a stored member's read 20 bytes early.
  const { root, archivePath, dest } = scratch(
    "extra",
    buildZip([{ name: "local-rag", data: PAYLOAD, localExtra: 20 }]),
  );
  extractSingleMember(archivePath, "local-rag", dest);
  assert.deepEqual(fs.readFileSync(dest), PAYLOAD);
  fs.rmSync(root, { recursive: true, force: true });
});

test("zip64 is refused by name rather than misparsed", () => {
  const { root, archivePath, dest } = scratch(
    "zip64",
    buildZip([{ name: "local-rag", data: PAYLOAD }], { zip64: true }),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("unsupported", /zip64/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a compression method other than stored or deflate is refused by number", () => {
  const { root, archivePath, dest } = scratch(
    "bzip2",
    buildZip([{ name: "local-rag", data: PAYLOAD, method: 12 }]),
  );
  assert.throws(
    () => extractSingleMember(archivePath, "local-rag", dest),
    isKind("unsupported", /method 12/),
  );
  fs.rmSync(root, { recursive: true, force: true });
});

test("a mode comes back from a Unix zip and null from a Windows one", () => {
  const unix = scratch("mode-unix", buildZip([{ name: "local-rag", data: PAYLOAD }]));
  assert.equal(extractSingleMember(unix.archivePath, "local-rag", unix.dest).mode, 0o755);
  fs.rmSync(unix.root, { recursive: true, force: true });

  // "version made by" 0x0014: made on FAT, where the external attributes are
  // DOS flags and no Unix mode exists to report.
  const dos = scratch(
    "mode-dos",
    buildZip([{ name: "local-rag", data: PAYLOAD, madeBy: 0x0014, mode: 0 }]),
  );
  assert.equal(extractSingleMember(dos.archivePath, "local-rag", dos.dest).mode, null);
  fs.rmSync(dos.root, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// Properties of the module as a whole
// ---------------------------------------------------------------------------

test("nothing is written to the destination when extraction fails", () => {
  for (const [label, archive] of [
    ["wrong-name", buildTar([{ name: "local-rag-proxy", data: PAYLOAD }])],
    ["two", buildTar([{ name: "a", data: "x" }, { name: "b", data: "y" }])],
    ["absolute", buildZip([{ name: "/local-rag", data: PAYLOAD }])],
    ["junk", Buffer.from("nope")],
  ]) {
    const { root, archivePath, dest } = scratch(`nowrite-${label}`, archive);
    assert.throws(() => extractSingleMember(archivePath, "local-rag", dest), ArchiveError, label);
    assert.equal(fs.existsSync(dest), false, label);
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("nothing in this module shells out", () => {
  const src = fs.readFileSync(path.join(__dirname, "..", "src", "archive.js"), "utf8");
  const code = src
    .split("\n")
    .filter((l) => !l.trimStart().startsWith("//") && !l.trimStart().startsWith("*"))
    .join("\n");
  assert.doesNotMatch(code, /require\(["']node:child_process["']\)/);
  assert.doesNotMatch(code, /\bexecFileSync\s*\(|\bexecSync\s*\(|\bspawnSync\s*\(/);
  assert.doesNotMatch(code, /require\(["']node:https?["']\)/);
});
