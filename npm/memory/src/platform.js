"use strict";

// Platform targets fixed by spec 13 §1 / 15 §2 `[FIXED]`. `win32-arm64` and
// FreeBSD are deferred — not a gap in this table, a normative boundary.
const SUPPORTED_PLATFORMS = Object.freeze([
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64",
  "linux-x64",
  "win32-x64",
]);

const DEFERRED_PLATFORMS = Object.freeze(["win32-arm64"]);

/**
 * @param {string} [platform] defaults to `process.platform`; a parameter
 *   only so tests can compute a key for a platform other than the host's
 *   without mutating global state.
 * @param {string} [arch] defaults to `process.arch`.
 * @returns {string} e.g. "darwin-arm64"
 */
function platformKey(platform = process.platform, arch = process.arch) {
  return `${platform}-${arch}`;
}

// Key -> Rust target triple, the names `cargo-dist` gives the release assets
// (`dist-workspace.toml`'s `targets`). Written as an explicit map rather than
// zipped positionally against that list on purpose: the two are ordered
// differently — `dist-workspace.toml` sorts by triple, this file sorts by key,
// so positions 2 and 3 disagree and a positional zip would quietly map
// `darwin-x64` onto `aarch64-unknown-linux-gnu`.
const TARGET_TRIPLES = Object.freeze({
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-arm64": "aarch64-unknown-linux-gnu",
  "linux-x64": "x86_64-unknown-linux-gnu",
  "win32-x64": "x86_64-pc-windows-msvc",
});

/**
 * @param {string} key e.g. "darwin-arm64"
 * @returns {string|null} the Rust target triple, or null for a key that has no
 *   release asset (unsupported, or the deferred `win32-arm64`).
 */
function targetTriple(key) {
  return Object.prototype.hasOwnProperty.call(TARGET_TRIPLES, key)
    ? TARGET_TRIPLES[key]
    : null;
}

/**
 * @param {string} [platform] defaults to `process.platform`.
 * @returns {string} `".exe"` on win32, `""` everywhere else.
 */
function exeSuffix(platform = process.platform) {
  return platform === "win32" ? ".exe" : "";
}

/**
 * @deprecated ADR-0013: there are no per-platform npm packages any more.
 *   Kept only because live callers still exist — `src/resolve.js` (retired by
 *   T22-09) and `plugin/test/mcp-launcher-tiers.test.js` (replaced by T22-12).
 *   Delete with the last of them, in T22-12; deleting it here would redden both
 *   suites for the whole npm branch.
 * @param {string} key e.g. "darwin-arm64"
 * @returns {string} e.g. "@13w/memory-darwin-arm64"
 */
function platformPackageName(key) {
  return `@13w/memory-${key}`;
}

/** @param {string} key @returns {boolean} */
function isSupported(key) {
  return SUPPORTED_PLATFORMS.includes(key);
}

/** @param {string} key @returns {boolean} */
function isDeferred(key) {
  return DEFERRED_PLATFORMS.includes(key);
}

module.exports = {
  SUPPORTED_PLATFORMS,
  DEFERRED_PLATFORMS,
  TARGET_TRIPLES,
  platformKey,
  targetTriple,
  exeSuffix,
  platformPackageName,
  isSupported,
  isDeferred,
};
