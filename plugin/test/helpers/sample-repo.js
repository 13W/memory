"use strict";

// A recursive, sorted snapshot of a directory tree — used to prove a "sample
// repository" is unchanged (not just "no `.claude/rules/`", which would miss
// any other unexpected write).
//
// The snapshot carries size and mtime, not names alone (T22-13). It used to
// return paths only, while its caller asserted "byte-for-byte unchanged" —
// which that snapshot could not support: a rewrite in place of an existing
// file changes no name and would have passed. Size and mtime do not literally
// prove byte equality either, but they are what a directory walk can honestly
// establish, and the claim is now stated to match.

const fs = require("node:fs");
const path = require("node:path");

/**
 * @param {string} root
 * @returns {Array<{path: string, kind: string, size: number, mtimeMs: number}>}
 *   one entry per file/directory below `root`, sorted by relative path.
 */
function listTree(root) {
  const out = [];
  function walk(dir) {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      // `lstat`, not `stat`: a symlink planted into the tree is itself the
      // write worth catching, and following it would report the target.
      const st = fs.lstatSync(full);
      out.push({
        path: path.relative(root, full),
        kind: entry.isDirectory() ? "dir" : entry.isSymbolicLink() ? "symlink" : "file",
        // A directory's own size and mtime move when an entry is added to or
        // removed from it, so they carry information here too.
        size: st.size,
        mtimeMs: st.mtimeMs,
      });
      if (entry.isDirectory()) {
        walk(full);
      }
    }
  }
  walk(root);
  return out.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
}

module.exports = { listTree };
