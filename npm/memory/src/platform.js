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

/**
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
  platformKey,
  platformPackageName,
  isSupported,
  isDeferred,
};
