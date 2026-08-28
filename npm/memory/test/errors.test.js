"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");

const {
  formatNotInstalledError,
  formatSourceCheckoutNotBuiltError,
  formatChecksumMismatchError,
  formatAssetAbsentError,
  formatDownloadError,
  formatOverrideMissingError,
} = require("../src/errors.js");

// The five `ResolveResult`-shaped formatters this file used to cover went
// with `src/resolve.js` in T22-11 — ADR-0013 leaves nothing for a "missing
// platform package" message to describe. Their one property that was not
// specific to them — a message is a single trimmed line, so it lands as one
// exit-1 diagnostic — is covered for the surviving six below by the pair of
// assertions in "keeps the local-rag: prefix and no trailing whitespace":
// `startsWith("local-rag: ")` rules out leading whitespace and `trimEnd()`
// rules out trailing, which together are the removed `msg === msg.trim()`.
// So nothing is lost with them. One of the removed tests deserves naming:
// "formatMissingPlatformError throws on an unknown reason" stayed green
// after the function was deleted — `assert.throws` happily caught the
// resulting TypeError. It was passing for the wrong reason at the moment it
// was removed.

// --- ADR-0013 formatters (T22-05) -----------------------------------------
//
// The contract every one of them owes: name what the reader needs in order to
// act, and end with exactly one runnable command. A message that explains a
// situation without saying what to type is the failure mode these replace.

const ADR_0013_MESSAGES = [
  [
    "not-installed",
    formatNotInstalledError({ key: "darwin-arm64" }),
    [/darwin-arm64/, /--ignore-scripts/, /LOCAL_RAG_BIN_DIR/],
  ],
  [
    "source-checkout-not-built",
    formatSourceCheckoutNotBuiltError({
      repoRoot: "/opt/soft/local-rag-v2",
      binary: "local-rag-proxy",
    }),
    [/\/opt\/soft\/local-rag-v2/, /local-rag-proxy/, /cargo build --release/],
  ],
  [
    "checksum-mismatch",
    formatChecksumMismatchError({
      asset: "local-rag-aarch64-apple-darwin.tar.gz",
      expected: "a".repeat(64),
      actual: "b".repeat(64),
    }),
    [/local-rag-aarch64-apple-darwin\.tar\.gz/, /a{64}/, /b{64}/, /Nothing was installed/],
  ],
  [
    "asset-absent",
    formatAssetAbsentError({ binary: "local-rag-tui", tag: "0.0.0", key: "linux-x64" }),
    [/local-rag-tui/, /0\.0\.0/, /linux-x64/],
  ],
  [
    "download",
    formatDownloadError({
      url: "https://example.test/releases/download/1.0.0/a.tar.gz",
      cause: "ETIMEDOUT",
    }),
    [/example\.test/, /ETIMEDOUT/, /LOCAL_RAG_BIN_DIR/],
  ],
  [
    "override-missing",
    formatOverrideMissingError({ dir: "/opt/bins", binary: "local-rag-hook" }),
    [/\/opt\/bins/, /local-rag-hook/, /LOCAL_RAG_BIN_DIR/],
  ],
];

for (const [name, msg, expectations] of ADR_0013_MESSAGES) {
  test(`${name} error names everything needed to act on it`, () => {
    for (const re of expectations) {
      assert.match(msg, re);
    }
  });

  test(`${name} error ends with exactly one runnable command`, () => {
    const lines = msg.split("\n");
    const fixIndex = lines.findIndex((l) => l.trim() === "Fix:");
    assert.notEqual(fixIndex, -1, "every message must have a Fix: section");
    const commands = lines.slice(fixIndex + 1).filter((l) => l.trim().length > 0);
    assert.equal(commands.length, 1, `expected one command, got ${commands.length}`);
    assert.match(commands[0], /^ {2}\S/, "the command is indented by exactly two spaces");
  });
}

test("the asset-absent message tells the truth about what is on disk (T22-17)", () => {
  // Found by running the installer against the real release, not by reading the
  // code: `latest` still resolves to tag 0.0.0, which predates T22-07's move to
  // `.tar.gz`, so every required binary is "absent" there. The message printed
  // said "The other binaries installed normally" while the directory held
  // nothing but `.local-rag-install.error` — false in the only case it is ever
  // shown, because `install.js` removes the whole scratch directory on failure.
  const required = formatAssetAbsentError({
    binary: "local-rag",
    tag: "0.0.0",
    key: "darwin-arm64",
    othersInstalled: false,
  });
  assert.match(required, /Nothing was installed/);
  assert.doesNotMatch(required, /other binaries installed normally/);
  // Reinstalling the package would resolve the same release, so advising it
  // here would be advice that cannot work.
  assert.doesNotMatch(required, /npm install --global @13w\/memory@latest/);
  assert.match(required, /LOCAL_RAG_BIN_DIR/);

  // `locate.js` reaches this only for an OPTIONAL binary, with the rest of the
  // install genuinely present — there the original sentence is true, and the
  // default keeps it.
  const optional = formatAssetAbsentError({
    binary: "local-rag-tui",
    tag: "0.0.0",
    key: "linux-x64",
  });
  assert.match(optional, /other binaries installed normally/);
  assert.match(optional, /npm install --global @13w\/memory@latest/);

  for (const msg of [required, optional]) {
    assert.ok(msg.startsWith("local-rag: "), msg);
    assert.equal(msg, msg.trimEnd(), msg);
  }
});

test("every ADR-0013 message keeps the local-rag: prefix and no trailing whitespace", () => {
  for (const [name, msg] of ADR_0013_MESSAGES) {
    assert.ok(msg.startsWith("local-rag: "), `${name} must carry the tool prefix`);
    assert.equal(msg, msg.trimEnd(), `${name} must not end in whitespace`);
  }
});

test("the checksum message shows both digests, so the reader can tell which is which", () => {
  const msg = formatChecksumMismatchError({
    asset: "a.tar.gz",
    expected: "a".repeat(64),
    actual: "b".repeat(64),
  });
  const expectedLine = msg.split("\n").find((l) => l.includes("expected"));
  const actualLine = msg.split("\n").find((l) => l.includes("actual"));
  assert.match(expectedLine, /a{64}/);
  assert.match(actualLine, /b{64}/);
  assert.doesNotMatch(expectedLine, /b{64}/);
});
