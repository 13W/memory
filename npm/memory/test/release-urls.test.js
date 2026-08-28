"use strict";

// Pure naming and parsing rules for the release assets (T22-05). Every case
// below is derived from bytes the real release actually serves, not from what
// the format ought to be: the sidecar shape and the redirect Location were both
// read off tag 0.0.0 before this module was written.

const { test } = require("node:test");
const assert = require("node:assert/strict");

const { SUPPORTED_PLATFORMS } = require("../src/platform.js");
const {
  DEFAULT_RELEASE_BASE_URL,
  PRODUCT_BINARIES,
  releaseBaseUrl,
  assetName,
  sidecarName,
  executableName,
  latestAssetUrl,
  pinnedAssetUrl,
  parseTagFromLocation,
  parseSha256Sidecar,
} = require("../src/release.js");

const REAL_ASSET = "local-rag-proxy-aarch64-apple-darwin.tar.xz";
const REAL_DIGEST =
  "9daab7a8b7b2cd7a82d553678d5edb37f642e378f120fe5b40194e1111a3b1bf";
// Byte-for-byte the form the real sidecar has: one space, an asterisk
// (coreutils binary mode), and a trailing blank line.
const REAL_SIDECAR = `${REAL_DIGEST} *${REAL_ASSET}\n\n`;
const REAL_LOCATION = `https://github.com/13W/memory/releases/download/0.0.0/${REAL_ASSET}`;

test("the base URL defaults to the project's own releases and drops a trailing slash", () => {
  assert.equal(releaseBaseUrl({}), DEFAULT_RELEASE_BASE_URL);
  assert.equal(
    releaseBaseUrl({ LOCAL_RAG_RELEASE_BASE_URL: "http://127.0.0.1:8080/r/" }),
    "http://127.0.0.1:8080/r",
  );
});

test("every supported platform names an asset, and only win32 uses .zip", () => {
  for (const key of SUPPORTED_PLATFORMS) {
    const name = assetName("local-rag-proxy", key);
    assert.ok(name, `${key} must name an asset`);
    const expected = key.startsWith("win32-") ? ".zip" : ".tar.gz";
    assert.ok(name.endsWith(expected), `${key} -> ${name} must end with ${expected}`);
  }
  assert.equal(assetName("local-rag", "win32-arm64"), null, "deferred key has no asset");
  assert.equal(assetName("local-rag", "freebsd-x64"), null);
});

// The producer's own archive formats, read rather than retyped, for the same
// reason `platform.test.js`'s sibling reader reads the target list: a hand-typed
// pair would only prove this file agrees with itself. If the two ever want a
// shared helper, that is where its twin lives.
function distWorkspaceArchiveFormats() {
  const toml = require("node:fs").readFileSync(
    require("node:path").join(__dirname, "..", "..", "..", "dist-workspace.toml"),
    "utf8",
  );
  const read = (key) => {
    const line = toml.split("\n").find((l) => l.trimStart().startsWith(key));
    assert.ok(line, `dist-workspace.toml must declare ${key}`);
    return /"([^"]+)"/.exec(line)[1];
  };
  return { unix: read("unix-archive"), windows: read("windows-archive") };
}

test("assetName's extensions are exactly dist-workspace.toml's archive formats", () => {
  // The producer and the consumer of these names are two files apart, and they
  // were deliberately out of step between T22-05 and T22-07. This is what keeps
  // them from drifting again — silently, and only noticed by a download that
  // 404s on a user's machine.
  const { unix, windows } = distWorkspaceArchiveFormats();
  for (const key of SUPPORTED_PLATFORMS) {
    const name = assetName("local-rag", key);
    const expected = key.startsWith("win32-") ? windows : unix;
    assert.ok(name.endsWith(expected), `${key} -> ${name} must end with ${expected}`);
  }
  // Both formats have to be ones `archive.js` can actually open.
  assert.equal(unix, ".tar.gz");
  assert.equal(windows, ".zip");
});

test("asset, sidecar and executable names agree with each other", () => {
  const name = assetName("local-rag-proxy", "darwin-arm64");
  assert.equal(name, "local-rag-proxy-aarch64-apple-darwin.tar.gz");
  assert.equal(sidecarName(name), `${name}.sha256`);
  assert.equal(executableName("local-rag-proxy", "linux"), "local-rag-proxy");
  assert.equal(executableName("local-rag-proxy", "win32"), "local-rag-proxy.exe");
});

test("latest and pinned URLs differ only in how the tag is named", () => {
  const env = { LOCAL_RAG_RELEASE_BASE_URL: "https://example.test/releases" };
  assert.equal(
    latestAssetUrl("a.tar.gz", env),
    "https://example.test/releases/latest/download/a.tar.gz",
  );
  assert.equal(
    pinnedAssetUrl("1.2.3", "a.tar.gz", env),
    "https://example.test/releases/download/1.2.3/a.tar.gz",
  );
});

test("the resolved tag is recovered from a real redirect Location", () => {
  assert.equal(parseTagFromLocation(REAL_LOCATION), "0.0.0");
  assert.equal(parseTagFromLocation(`${REAL_LOCATION}?actor_id=1&key=x`), "0.0.0");
  assert.equal(parseTagFromLocation(`${REAL_LOCATION}#frag`), "0.0.0");
  assert.equal(
    parseTagFromLocation("https://example.test/releases/download/v2.0.0-rc.1/a.zip"),
    "v2.0.0-rc.1",
  );
});

test("a mirror's Location parses too, not just github.com's (D-109)", () => {
  // The `/releases` segment belongs to the base URL, not to the URL shape
  // `pinnedAssetUrl` builds. Requiring it made every non-GitHub base — the
  // documented mirror and air-gapped paths — unparseable; the loopback fixture
  // server in `http-fetch.test.js` is the first caller that proved it.
  assert.equal(
    parseTagFromLocation("http://127.0.0.1:65094/download/2.3.4/a.tar.gz"),
    "2.3.4",
  );
  assert.equal(
    parseTagFromLocation(pinnedAssetUrl("9.9.9", "a.tar.gz", {
      LOCAL_RAG_RELEASE_BASE_URL: "https://mirror.example.test/lr",
    })),
    "9.9.9",
    "the parser must agree with the builder for any base",
  );
});

test("a Location that is not a release download is an error, never a silent null", () => {
  for (const bad of [
    "https://github.com/13W/memory/releases/tag/0.0.0",
    "https://example.test/nowhere",
    "",
  ]) {
    assert.throws(() => parseTagFromLocation(bad), /local-rag:/);
  }
  assert.throws(() => parseTagFromLocation(undefined), /local-rag:/);
});

test("the real sidecar form parses: <64 hex> *<name>, trailing blank line and all", () => {
  assert.equal(parseSha256Sidecar(REAL_SIDECAR, REAL_ASSET), REAL_DIGEST);
});

test("the two-space form and a bare digest parse too", () => {
  assert.equal(
    parseSha256Sidecar(`${REAL_DIGEST}  ${REAL_ASSET}\n`, REAL_ASSET),
    REAL_DIGEST,
  );
  assert.equal(parseSha256Sidecar(`${REAL_DIGEST}\n`, REAL_ASSET), REAL_DIGEST);
});

test("an uppercase digest is normalised to lowercase hex", () => {
  assert.equal(
    parseSha256Sidecar(`${REAL_DIGEST.toUpperCase()} *${REAL_ASSET}\n`, REAL_ASSET),
    REAL_DIGEST,
  );
});

test("a sidecar certifying a different file is rejected, not accepted by position", () => {
  const other = "local-rag-hook-aarch64-apple-darwin.tar.xz";
  assert.throws(
    () => parseSha256Sidecar(`${REAL_DIGEST} *${other}\n`, REAL_ASSET),
    /certifies a different file/,
  );
});

test("a digest that is not 64 hex characters is rejected", () => {
  for (const bad of [REAL_DIGEST.slice(0, 63), `${REAL_DIGEST}a`, "z".repeat(64)]) {
    assert.throws(
      () => parseSha256Sidecar(`${bad} *${REAL_ASSET}\n`, REAL_ASSET),
      /local-rag:/,
      `"${bad.slice(0, 12)}…" must be rejected`,
    );
  }
});

test("an empty or non-text sidecar is rejected", () => {
  assert.throws(() => parseSha256Sidecar("", REAL_ASSET), /is empty/);
  assert.throws(() => parseSha256Sidecar("\n\n  \n", REAL_ASSET), /is empty/);
  assert.throws(() => parseSha256Sidecar(null, REAL_ASSET), /was not text/);
});

/**
 * Every crate `cargo-dist` would ship, read out of the workspace instead of
 * retyped. A crate is shipped when it carries `[package.metadata.dist]` with
 * `dist = true` — that, and not `publish`, is the switch here: every crate in
 * this workspace is `publish = false` (the root's `[workspace.package]`), so
 * `publish` says nothing about the release, and `xtask` is excluded precisely
 * because it has no such section.
 */
function distShippedCrates() {
  const fs = require("node:fs");
  const path = require("node:path");
  const cratesDir = path.join(__dirname, "..", "..", "..", "crates");
  const shipped = [];
  for (const entry of fs.readdirSync(cratesDir)) {
    const manifest = path.join(cratesDir, entry, "Cargo.toml");
    if (!fs.existsSync(manifest)) continue;
    const toml = fs.readFileSync(manifest, "utf8");
    const section = /\[package\.metadata\.dist\]([\s\S]*?)(?=\n\[|$)/.exec(toml);
    if (section === null || !/^\s*dist\s*=\s*true\s*$/m.test(section[1])) continue;
    const name = /^\s*name\s*=\s*"([^"]+)"/m.exec(toml);
    assert.ok(name, `${entry}/Cargo.toml must declare a package name`);
    shipped.push(name[1]);
  }
  return shipped;
}

test("PRODUCT_BINARIES is exactly what cargo-dist would ship (T22-17)", () => {
  // The other half of "the release produces exactly what the installer
  // consumes". `platform.test.js` already holds the *triples* to
  // `dist-workspace.toml`; nothing held the *binaries* to anything, and that is
  // the half that actually moved: release 0.0.0 carries three binaries because
  // it was cut before `local-rag-tui` existed, while this list has four.
  //
  // A mismatch is invisible until a user's install 404s (a name the release
  // does not carry) or silently ships less than it could (a crate the installer
  // never asks for) — `local-rag-tui` spent three weeks in the second state.
  assert.deepEqual(
    PRODUCT_BINARIES.map((b) => b.name).sort(),
    distShippedCrates().sort(),
  );
});

test("only local-rag-tui is optional, and it is the one the old release lacks", () => {
  // `required: false` is load-bearing rather than decorative: `install.js`
  // records an absent optional binary and carries on, which is what lets the
  // installer work against a tag cut before that binary existed. Pinning it
  // here means dropping the distinction shows up as a failure and not as a
  // mysteriously tolerant install.
  const optional = PRODUCT_BINARIES.filter((b) => !b.required).map((b) => b.name);
  assert.deepEqual(optional, ["local-rag-tui"]);
});

test("nothing in this module reaches the network", () => {
  const src = require("node:fs").readFileSync(
    require("node:path").join(__dirname, "..", "src", "release.js"),
    "utf8",
  );
  const code = src
    .split("\n")
    .filter((l) => !l.trimStart().startsWith("//") && !l.trimStart().startsWith("*"))
    .join("\n");
  assert.doesNotMatch(code, /require\(["']node:https?["']\)/);
  assert.doesNotMatch(code, /\bfetch\s*\(/);
});
