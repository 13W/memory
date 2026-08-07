"use strict";

// Locates a cargo-built native binary for tests that want genuine
// end-to-end coverage of the real Rust binary through this plugin's JS
// bootstrap/cache layer, not a stub. Gracefully absent (not built) rather
// than an error — gate tests on this, same idiom as `claude-availability.js`.
// Generalized (T19-03; was `native-hook-binary.js`, hook-only) — the MCP
// launcher's own cold-start test needs the same lookup for
// `local-rag-proxy`.

const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");

/**
 * @param {string} name - e.g. `"local-rag-hook"` or `"local-rag-proxy"`.
 * @returns {string|null} absolute path, or null if not built yet.
 */
function nativeBinaryPath(name) {
  const suffix = process.platform === "win32" ? ".exe" : "";
  const p = path.join(REPO_ROOT, "target", "debug", name + suffix);
  return fs.existsSync(p) ? p : null;
}

module.exports = { nativeBinaryPath, REPO_ROOT };
