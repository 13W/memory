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
