"use strict";

// A recursive, sorted snapshot of a directory tree — used to prove a
// "sample repository" is byte-for-byte unchanged (not just "no
// `.claude/rules/`", which would miss any other unexpected write).

const fs = require("node:fs");
const path = require("node:path");

/**
 * @param {string} root
 * @returns {string[]} paths relative to `root`, sorted.
 */
function listTree(root) {
  const out = [];
  function walk(dir) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      out.push(path.relative(root, full));
      if (entry.isDirectory()) {
        walk(full);
      }
    }
  }
  walk(root);
  return out.sort();
}

module.exports = { listTree };
