#!/usr/bin/env node
"use strict";

// npm's `postinstall`, and the one rule that outranks everything else here: it
// exits 0 no matter what happens.
//
// That is not the same as pretending. `scripts/install.js` exits non-zero when
// an install fails and says why — an installer that reports success it did not
// achieve cannot be debugged afterwards. This wrapper is where the *other*
// decision is made: whether a failed binary download should fail the user's
// whole `npm install`. It should not. The package still works: every command
// ships a stub that resolves and, on the MCP path, heals on first use.
//
// The branches below are the cases where there is nothing to do, and each says
// so rather than trying anyway:
//
//   - `LOCAL_RAG_BIN_DIR` is set. ADR-0013 makes it the air-gapped answer,
//     "which wins over everything and never downloads" — downloading here would
//     contradict the variable's whole purpose.
//   - We are inside a source checkout. The local build is the point; a release
//     from some other day would not match the source sitting right there.
//   - The platform has no release asset.
//   - There is nowhere to write (no `LOCAL_RAG_HOME`, no XDG base, no home).
//
// `--ignore-scripts` needs no branch: it means this file never runs, which is
// exactly why the stubs can heal by themselves.

const { platformKey } = require("../src/platform");
const { installDir, PathsError } = require("../src/paths");
const { sourceCheckoutRoot } = require("../src/locate");
const { installBinaries } = require("../src/install");
const { replaceShims } = require("../src/replace-shims");

const PACKAGE_DIR = require("node:path").resolve(__dirname, "..");
const BIN_DIR_VAR = "LOCAL_RAG_BIN_DIR";

function say(line) {
  process.stdout.write(`local-rag: ${line}\n`);
}

async function main() {
  const env = process.env;

  if (typeof env[BIN_DIR_VAR] === "string" && env[BIN_DIR_VAR] !== "") {
    say(`${BIN_DIR_VAR} is set; using the binaries there and downloading nothing`);
    return;
  }

  const checkout = sourceCheckoutRoot(PACKAGE_DIR);
  if (checkout !== null) {
    say(`running from the source checkout at ${checkout}; build it with cargo, not npm`);
    return;
  }

  const key = platformKey();
  let dir;
  try {
    dir = installDir(key, env);
  } catch (err) {
    if (err instanceof PathsError) {
      say(err.message);
      return;
    }
    throw err;
  }

  const report = await installBinaries(dir, {
    // The default mode: re-resolve `latest` and refetch when the tag moved.
    // That is what makes "update the package, get the new services" true
    // (ADR-0013 Decision 2) — this script re-runs on every install and update.
    mode: "update",
    dirMode: 0o700,
    log: say,
  });
  if (report.skipped) {
    say(`${report.reason} (${report.tag ?? "no tag"})`);
  }

  const swapped = replaceShims(PACKAGE_DIR, dir, { log: say });
  if (!swapped.skipped && swapped.failed.length > 0) {
    say(`kept the stub for: ${swapped.failed.join(", ")}`);
  }
}

main().then(
  () => process.exit(0),
  (err) => {
    process.stderr.write(`${err && err.message ? err.message : err}\n`);
    process.stderr.write(
      // Not "or by `local-rag-install`", which this said until T22-14: no such
      // command exists — `bin` maps six names and that is not one of them.
      // Pointing a user at a command that is not there is worse than saying
      // nothing, and the healing path is real (see `src/shim.js`).
      "local-rag: the package is installed; the binaries are not. " +
        "They will be fetched on first use.\n",
    );
    process.exit(0);
  },
);
