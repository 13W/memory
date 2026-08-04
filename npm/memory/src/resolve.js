"use strict";

const path = require("node:path");
const { createRequire } = require("node:module");

const {
  SUPPORTED_PLATFORMS,
  platformKey,
  platformPackageName,
  isSupported,
  isDeferred,
} = require("./platform");

/**
 * @typedef {
 *   {ok: true, key: string, packageName: string, packageDir: string} |
 *   {ok: false, reason: 'unsupported'|'deferred'|'not-installed', key: string, packageName: string|null}
 * } ResolveResult
 */

/**
 * Resolve the platform package for the current (or an injected) host,
 * honoring whatever npm/pnpm/yarn layout actually installed it.
 *
 * `require.resolve`'s own directory-walk (via `createRequire(fromFile)`,
 * anchored at the caller's real, symlink-resolved location) IS the
 * hoisting-aware algorithm every package manager already targets — this
 * function never walks `node_modules` by hand, so pnpm's non-hoisting,
 * symlinked layout, yarn classic's flatter one, and plain npm all resolve
 * correctly with no special-casing.
 *
 * @param {string} fromFile - anchor for `createRequire`; production passes
 *   the launcher's own `__filename` (its real on-disk location, e.g.
 *   `bin/local-rag-mcp.js`), tests pass a synthetic path inside a fixture
 *   tree so this stays exercisable without any real `npm install`.
 * @param {{platform?: string, arch?: string}} [opts]
 * @returns {ResolveResult}
 */
function resolvePlatformPackage(fromFile, opts = {}) {
  const platform = opts.platform ?? process.platform;
  const arch = opts.arch ?? process.arch;
  const key = platformKey(platform, arch);

  if (isDeferred(key)) {
    return { ok: false, reason: "deferred", key, packageName: platformPackageName(key) };
  }
  if (!isSupported(key)) {
    return { ok: false, reason: "unsupported", key, packageName: null };
  }

  const packageName = platformPackageName(key);
  const requireFromLauncher = createRequire(fromFile);
  let packageJsonPath;
  try {
    packageJsonPath = requireFromLauncher.resolve(`${packageName}/package.json`);
  } catch (err) {
    if (err && err.code === "MODULE_NOT_FOUND") {
      return { ok: false, reason: "not-installed", key, packageName };
    }
    throw err;
  }

  return { ok: true, key, packageName, packageDir: path.dirname(packageJsonPath) };
}

/**
 * Fixed, manifest-free convention — mirrors `local-rag-proxy`'s own
 * `resolve_daemon_binary_path` (`crates/local-rag-proxy/src/connect.rs`),
 * which finds `local-rag` by flat, no-env, no-manifest directory lookup
 * next to itself. Every product binary lives in one flat `bin/` directory
 * inside the platform package.
 *
 * @param {string} packageDir - absolute path to the resolved platform
 *   package's root (i.e. `ResolveResult.packageDir` on an `ok:true` result).
 * @param {string} platform - `process.platform`-shaped value; only `"win32"`
 *   changes the suffix.
 * @param {'local-rag'|'local-rag-proxy'|'local-rag-hook'} name
 * @returns {string} absolute path, e.g. ".../bin/local-rag-proxy"
 */
function binaryPath(packageDir, platform, name) {
  const suffix = platform === "win32" ? ".exe" : "";
  return path.join(packageDir, "bin", name + suffix);
}

module.exports = {
  SUPPORTED_PLATFORMS,
  resolvePlatformPackage,
  binaryPath,
};
