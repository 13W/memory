#!/usr/bin/env node
"use strict";

// Refuses to pack a package whose `bin/` has been through an install.
//
// The whole design has `postinstall` replace the JS stubs in `bin/` with native
// binaries. On a maintainer's machine that is the same directory `npm pack`
// reads — so without this, publishing from a checkout that has ever been
// installed would put a hundred megabytes of one platform's binaries into a
// tarball that every platform downloads, and would ship them as if they were
// the package's own source.
//
// The rule is a whitelist rather than a blacklist of formats: every file in
// `bin/` must be a script, i.e. must start with `#!`. A Mach-O, an ELF, a PE,
// and anything else nobody has thought of all fail that test at once. The
// installer's own bookkeeping — the manifest, the lock, a scratch directory —
// is named explicitly on top, because those are text and would otherwise pass.
//
// `prepack` runs for `npm pack`, `npm pack --dry-run` and `npm publish`, and a
// non-zero exit aborts the pack before a tarball exists. Verified against npm's
// own source (`libnpmpack/lib/index.js` awaits the `prepack` script before
// `pacote.tarball`, and `dryRun` gates only the final `writeFile`) and by
// running it. `--ignore-scripts` is the one way past it, which is the user
// explicitly asking.

const fs = require("node:fs");
const path = require("node:path");

const PACKAGE_DIR = path.resolve(__dirname, "..");
const BIN_DIR = path.join(PACKAGE_DIR, "bin");
const INSTALL_ARTEFACT = /^\.local-rag-install\./;

/**
 * @returns {string[]} one line per reason this must not be packed
 */
function findInstallArtefacts(binDir = BIN_DIR) {
  const complaints = [];
  let entries;
  try {
    entries = fs.readdirSync(binDir, { withFileTypes: true });
  } catch (err) {
    return [`cannot read ${binDir}: ${err.message}`];
  }

  for (const entry of entries) {
    const full = path.join(binDir, entry.name);
    if (INSTALL_ARTEFACT.test(entry.name)) {
      complaints.push(`${entry.name} is an installer artefact, not package content`);
      continue;
    }
    if (entry.isDirectory()) {
      complaints.push(`${entry.name}/ is a directory; bin/ holds only entry points`);
      continue;
    }
    if (!entry.isFile()) {
      complaints.push(`${entry.name} is not a regular file`);
      continue;
    }
    let head;
    try {
      const fd = fs.openSync(full, "r");
      const buf = Buffer.alloc(2);
      fs.readSync(fd, buf, 0, 2, 0);
      fs.closeSync(fd);
      head = buf;
    } catch (err) {
      complaints.push(`${entry.name} could not be read: ${err.message}`);
      continue;
    }
    if (head[0] !== 0x23 || head[1] !== 0x21) {
      const size = fs.statSync(full).size;
      complaints.push(
        `${entry.name} does not start with "#!" — a native binary landed here (${size} bytes)`,
      );
    }
  }
  return complaints;
}

function main() {
  const complaints = findInstallArtefacts();
  if (complaints.length === 0) return 0;
  process.stderr.write("local-rag: refusing to pack — bin/ has been installed into.\n");
  for (const complaint of complaints) {
    process.stderr.write(`  ${complaint}\n`);
  }
  process.stderr.write(
    "Fix:\n  git clean -xdf npm/memory/bin && git checkout -- npm/memory/bin\n",
  );
  return 1;
}

if (require.main === module) {
  process.exit(main());
}

module.exports = { findInstallArtefacts, BIN_DIR };
