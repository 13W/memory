"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

/**
 * A fresh temp directory, already run through `realpathSync` — on macOS
 * `os.tmpdir()` lives under `/var`, itself a symlink to `/private/var`;
 * `require.resolve`'s own results are always the fully-resolved real path,
 * so fixture roots must be normalized the same way up front, or every
 * downstream path comparison in a test spuriously mismatches by prefix
 * even though resolution behaved correctly.
 *
 * @param {string} [prefix]
 * @returns {string}
 */
function mkTmpRoot(prefix = "lr-test-") {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  return fs.realpathSync(dir);
}

module.exports = { mkTmpRoot };
