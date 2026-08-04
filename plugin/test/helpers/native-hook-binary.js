"use strict";

// Locates the cargo-built `local-rag-hook` binary for tests that want
// genuine end-to-end coverage of the real Rust binary through this
// plugin's new JS bootstrap/cache layer, not a stub. Gracefully absent
// (not built) rather than an error — gate tests on this, same idiom as
// `claude-availability.js`.

const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..", "..");

/** @returns {string|null} absolute path, or null if not built yet. */
function nativeHookBinaryPath() {
  const suffix = process.platform === "win32" ? ".exe" : "";
  const p = path.join(REPO_ROOT, "target", "debug", "local-rag-hook" + suffix);
  return fs.existsSync(p) ? p : null;
}

module.exports = { nativeHookBinaryPath, REPO_ROOT };
