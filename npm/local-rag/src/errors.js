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

/** @param {Extract<import('./resolve').ResolveResult, {ok:false}>} result */
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

module.exports = {
  formatMissingPlatformError,
  formatUnsupportedPlatformError,
  formatDeferredPlatformError,
  formatMissingPackageError,
};
