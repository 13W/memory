"use strict";

// Best-effort direct-exec cache, shared by the hook launcher (T17-02, spec
// 13 §1's "<50ms cold" hook budget) and the MCP launcher (T19-03, spec 13
// §1/§2's own MCP cold-start budget). `${CLAUDE_PLUGIN_DATA}` is Claude
// Code's own persistent-per-plugin directory — survives plugin updates
// (unlike `${CLAUDE_PLUGIN_ROOT}`), the same place the official docs' own
// example caches an `npm install` across sessions. Refreshing a plain
// symlink here once lets every subsequent invocation exec the native
// binary directly (a bare `kill(2)`-class process spawn), skipping
// Node/npx entirely on the steady-state path. Named `binary-cache.js`
// (T19-03; was `hook-cache.js`) — the cache key is now the binary name,
// not implicitly "the hook", since `local-rag-hook` and `local-rag-proxy`
// share this same `${CLAUDE_PLUGIN_DATA}/bin/` directory.

const fs = require("node:fs");
const path = require("node:path");

/**
 * @param {string} pluginData - `${CLAUDE_PLUGIN_DATA}` value.
 * @param {string} name - the binary's own name, e.g. `"local-rag-hook"` or
 *   `"local-rag-proxy"` (no extension — the platform suffix is added here).
 * @param {string} [platform] - defaults to `process.platform`; a parameter
 *   only so tests can compute a path for a platform other than the host's.
 * @returns {string} absolute path to the cached symlink location.
 */
function cachedBinaryPath(pluginData, name, platform = process.platform) {
  const suffix = platform === "win32" ? ".exe" : "";
  return path.join(pluginData, "bin", name + suffix);
}

/**
 * Idempotently point `cachedBinaryPath(pluginData, name, platform)` at
 * `execPath`. Never throws — a failed refresh must never fail the caller
 * that triggered it; the next invocation falls back to its own slower path
 * (hook: `npx` via `hooks.json`'s own `||` chain; MCP: this same cache
 * simply staying empty, tier 3 of `local-rag-mcp-launcher.js`) regardless
 * of whether this succeeded.
 *
 * On platforms where `fs.symlinkSync` needs elevated privileges (notably
 * Windows without Developer Mode), this silently no-ops — every invocation
 * then takes the slow path, which still runs correctly, just without the
 * fast-path budget. Documented limitation (T17-02), same class of Windows
 * deferral as the daemon's own named-pipe gap (group 16) — verified once
 * T17-03 stands up Windows CI.
 *
 * @param {string} execPath - the resolved, real native binary path.
 * @param {string} name - see `cachedBinaryPath`.
 * @param {{pluginData?: string, platform?: string}} [opts]
 */
function refreshCache(execPath, name, opts = {}) {
  const pluginData = opts.pluginData ?? process.env.CLAUDE_PLUGIN_DATA;
  if (!pluginData) {
    return; // not invoked as a plugin hook/MCP server (e.g. a manual/test run) — nothing to cache
  }
  const platform = opts.platform ?? process.platform;
  try {
    const cachedPath = cachedBinaryPath(pluginData, name, platform);
    fs.mkdirSync(path.dirname(cachedPath), { recursive: true });

    let current = null;
    try {
      current = fs.readlinkSync(cachedPath);
    } catch {
      current = null; // missing, not a symlink, or unreadable — treat as stale
    }
    if (current === execPath) {
      return; // already up to date
    }

    const tmpPath = `${cachedPath}.tmp-${process.pid}`;
    fs.symlinkSync(execPath, tmpPath);
    fs.renameSync(tmpPath, cachedPath); // atomic replace: never a window with no file at all
  } catch {
    // Best-effort by design — see doc comment above.
  }
}

module.exports = { cachedBinaryPath, refreshCache };
