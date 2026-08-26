"use strict";

const { SUPPORTED_PLATFORMS } = require("./platform");

/** @param {Extract<import('./resolve').ResolveResult, {ok:false}>} result */
function formatUnsupportedPlatformError(result) {
  return (
    `local-rag: no prebuilt binary is available for this platform (${result.key}).\n` +
    `Supported platforms: ${SUPPORTED_PLATFORMS.join(", ")}.`
  );
}

/** @param {Extract<import('./resolve').ResolveResult, {ok:false}>} result */
function formatDeferredPlatformError(result) {
  return (
    `local-rag: no prebuilt binary is available for this platform (${result.key}).\n` +
    `Supported platforms: ${SUPPORTED_PLATFORMS.join(", ")}.\n` +
    `${result.key} support is planned but not yet available.`
  );
}

/**
 * @deprecated ADR-0013: there is no platform package to be missing. Kept while
 *   the three `bin/` shims still call it through `formatMissingPlatformError`;
 *   they are rewritten by T22-10, which deletes both. `subprocess.test.js`
 *   asserts the real shim's stderr against this text, so it must keep the
 *   `local-rag: ` prefix and the platform key until then.
 * @param {Extract<import('./resolve').ResolveResult, {ok:false}>} result
 */
function formatMissingPackageError(result) {
  return (
    `local-rag: the platform package "${result.packageName}" for ${result.key} is not installed.\n` +
    "This usually means optional dependencies were skipped during install\n" +
    '(e.g. "npm install --omit=optional", "--no-optional", a lockfile mismatch,\n' +
    "or a registry/network failure).\n" +
    "Fix: reinstall without omitting optional dependencies, or run\n" +
    `  npm install ${result.packageName} --save-optional`
  );
}

/**
 * @deprecated ADR-0013 — see `formatMissingPackageError`. Deleted by T22-10
 *   together with its three `bin/` callers.
 * @param {Extract<import('./resolve').ResolveResult, {ok:false}>} result
 * @returns {string}
 */
function formatMissingPlatformError(result) {
  switch (result.reason) {
    case "unsupported":
      return formatUnsupportedPlatformError(result);
    case "deferred":
      return formatDeferredPlatformError(result);
    case "not-installed":
      return formatMissingPackageError(result);
    default:
      throw new Error(`local-rag: unknown resolve failure reason: ${result.reason}`);
  }
}

// ---------------------------------------------------------------------------
// ADR-0013 formatters. Unlike the four above these take plain arguments rather
// than a `ResolveResult`, so this module stops depending on the shape of
// `resolve.js` — which T22-09 renames. Every message names what is needed to
// act and ends with exactly one runnable command, never advice.
// ---------------------------------------------------------------------------

/**
 * @param {{key: string, installCommand?: string, binDirVar?: string}} o
 * @returns {string}
 */
function formatNotInstalledError({
  key,
  installCommand = "npm install --global @13w/memory",
  binDirVar = "LOCAL_RAG_BIN_DIR",
}) {
  return (
    `local-rag: the native binaries for ${key} are not installed.\n` +
    `This happens when the package's install step did not run — for example\n` +
    `"npm ci --ignore-scripts", pnpm's default policy for dependency scripts,\n` +
    "or Yarn PnP — or when an offline install has not been pointed at a\n" +
    `directory of prebuilt binaries via ${binDirVar}.\n` +
    "Fix:\n" +
    `  ${installCommand}`
  );
}

/**
 * @param {{repoRoot: string, binary: string, crate?: string}} o
 * @returns {string}
 */
function formatSourceCheckoutNotBuiltError({ repoRoot, binary, crate = binary }) {
  return (
    `local-rag: running from the source checkout at ${repoRoot}, ` +
    `but ${binary} has not been built there.\n` +
    "Nothing is downloaded in a checkout — the local build is the point.\n" +
    "Fix:\n" +
    `  cargo build --release -p ${crate}`
  );
}

/**
 * @param {{asset: string, expected: string, actual: string}} o
 * @returns {string}
 */
function formatChecksumMismatchError({ asset, expected, actual }) {
  return (
    `local-rag: ${asset} does not match its published checksum.\n` +
    `  expected ${expected}\n` +
    `  actual   ${actual}\n` +
    "Nothing was installed. This is a corrupt or truncated download far more\n" +
    "often than an attack, and retrying is the first thing to try.\n" +
    "Fix:\n" +
    "  npm install --global @13w/memory"
  );
}

/**
 * @param {{binary: string, tag: string, key: string}} o
 * @returns {string}
 */
function formatAssetAbsentError({ binary, tag, key }) {
  return (
    `local-rag: release ${tag} ships no ${binary} for ${key}.\n` +
    "The other binaries installed normally; this one is unavailable in that\n" +
    "release rather than broken here.\n" +
    "Fix:\n" +
    "  npm install --global @13w/memory@latest"
  );
}

/**
 * @param {{url: string, cause: string}} o
 * @returns {string}
 */
function formatDownloadError({ url, cause }) {
  return (
    `local-rag: could not download ${url}\n` +
    `  ${cause}\n` +
    "Behind a proxy, the npm proxy settings are honoured; for an offline\n" +
    "install, point LOCAL_RAG_BIN_DIR at a directory of prebuilt binaries.\n" +
    "Fix:\n" +
    "  npm install --global @13w/memory"
  );
}

/**
 * @param {{dir: string, binary: string, envVar?: string}} o
 * @returns {string}
 */
function formatOverrideMissingError({ dir, binary, envVar = "LOCAL_RAG_BIN_DIR" }) {
  return (
    `local-rag: ${envVar} is set to ${dir}, but ${binary} is not an executable there.\n` +
    `${envVar} is an explicit override: it is never silently ignored, and nothing\n` +
    "is downloaded to make up for it.\n" +
    "Fix:\n" +
    `  unset ${envVar}`
  );
}

module.exports = {
  formatMissingPlatformError,
  formatUnsupportedPlatformError,
  formatDeferredPlatformError,
  formatMissingPackageError,
  formatNotInstalledError,
  formatSourceCheckoutNotBuiltError,
  formatChecksumMismatchError,
  formatAssetAbsentError,
  formatDownloadError,
  formatOverrideMissingError,
};
