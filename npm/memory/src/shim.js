"use strict";

// What runs when the native binary is not there yet.
//
// After a normal install these stubs are gone: `replace-shims.js` puts the
// native file at the very path npm's `.bin/` entry points at, so the command
// execs a binary with no Node in the way. This module is what happens on every
// path where that replacement did not run — `npm ci --ignore-scripts`, pnpm's
// default policy for dependency scripts, a `pnpm link --global` from a
// checkout, Windows, Yarn — and its job is to make those work anyway rather
// than to explain why they do not.
//
// THREE MODES, AND THE DIFFERENCE IS STRUCTURAL.
//
// The hook is fail-open by `[FIXED]` contract (11 §3.1: seven events, always
// exit 0) with a <50 ms cold budget (13 §1). It cannot wait for a download, so
// it starts a detached repair and gets out of the way. It also cannot afford to
// throw: the contract covers the whole command a `hooks.json` entry invokes,
// and a top-level `require` that throws is not fail-open — which is why each
// stub wraps its own entry in a `try`.
//
// The MCP proxy can afford to wait, and must: `.mcp.json` starting a server
// that is not installed has nowhere else to heal. So it repairs synchronously —
// ADR-0013 Decision 1's "lazy path that heals on first use when the lifecycle
// script did not run". Its stdout is the JSON-RPC stream (13 §2: "stdout stays
// byte-empty"), so every byte this module emits goes to stderr, and the repair
// child's stdout is discarded rather than inherited.
//
// The CLI mode (`local-rag`, `local-rag-tui`) is the MCP one without the
// stdout taboo. It still writes diagnostics to stderr, because a diagnostic on
// stdout would land in whatever the user was piping the command into.
//
// WHAT IT REPAIRS AND WHAT IT REFUSES TO. Only a plain "not installed" is
// healed. `LOCAL_RAG_BIN_DIR` pointing somewhere empty is an explicit
// instruction that ADR-0013 says "wins over everything and never downloads",
// and a checkout with nothing built wants `cargo build`, not a release from
// some other day. `locate.js` already distinguishes those, and this module
// simply believes it.

const path = require("node:path");
const fs = require("node:fs");
const { constants } = require("node:os");
const { spawn, spawnSync } = require("node:child_process");

const { locateBinary } = require("./locate");
const { runAndForwardSignals } = require("./lifecycle");

const INSTALL_SCRIPT = path.resolve(__dirname, "..", "scripts", "install.js");

/** Only this one failure is a missing install; the others are instructions. */
function isHealable(result) {
  return result.reason === "not-installed";
}

/**
 * Start a repair that outlives us.
 *
 * `--no-wait` because seven hook events firing at once would otherwise queue
 * seven installers behind one lock, each holding a process open for the whole
 * download. The first one wins; the rest find the lock held and leave.
 */
function startDetachedRepair() {
  if (!fs.existsSync(INSTALL_SCRIPT)) return;
  try {
    const child = spawn(process.execPath, [INSTALL_SCRIPT, "--if-needed", "--no-wait"], {
      detached: true,
      stdio: "ignore",
    });
    child.unref();
  } catch {
    // A repair that cannot start is not a reason to fail the caller.
  }
}

/**
 * Repair and wait. stdout is discarded rather than inherited — on the MCP path
 * it belongs to the protocol, and on the others a progress line has no business
 * in a pipeline.
 */
function repairSynchronously() {
  if (!fs.existsSync(INSTALL_SCRIPT)) return;
  try {
    spawnSync(process.execPath, [INSTALL_SCRIPT, "--if-needed"], {
      stdio: ["ignore", "ignore", "inherit"],
    });
  } catch {
    // The retry below reports whatever state we are actually in.
  }
}

/**
 * The hook path: never throws, never waits, never writes to stdout.
 *
 * @param {string} binary "local-rag-hook"
 * @param {object} [opts] test seam; `locate` overrides the resolver
 */
function runHookShim(binary, opts = {}) {
  const locate = opts.locate ?? locateBinary;
  const located = locate(binary, opts.locateOptions ?? {});
  if (!located.ok) {
    if (isHealable(located)) startDetachedRepair();
    process.stderr.write(`${located.message}\n`);
    process.exit(0);
    return;
  }

  const child = spawnSync(located.path, process.argv.slice(2), { stdio: "inherit" });
  if (child.error) {
    process.stderr.write(`local-rag: could not run ${located.path}: ${child.error.message}\n`);
    process.exit(0);
    return;
  }
  // Passed through rather than hardcoded: `local-rag-hook version` must report
  // its real code, and a child killed by a signal (`status === null`) still
  // falls back to 0 rather than propagating null.
  process.exit(child.status ?? 0);
}

/**
 * The MCP and CLI paths: heal, then run, forwarding signals to the child.
 *
 * @param {string} binary
 * @param {object} [opts] test seam; `locate`, `repair`, `run` are injectable
 */
async function runManagedShim(binary, opts = {}) {
  const locate = opts.locate ?? locateBinary;
  const repair = opts.repair ?? repairSynchronously;
  const run = opts.run ?? runAndForwardSignals;
  const locateOptions = opts.locateOptions ?? {};

  let located = locate(binary, locateOptions);
  if (!located.ok && isHealable(located)) {
    repair();
    located = locate(binary, locateOptions);
  }
  if (!located.ok) {
    process.stderr.write(`${located.message}\n`);
    process.exit(1);
    return;
  }

  try {
    const { code, signal } = await run(located.path, process.argv.slice(2));
    if (signal) {
      const signalNumber = constants.signals[signal];
      process.stderr.write(`local-rag: the native binary was terminated by ${signal}\n`);
      process.exit(128 + (signalNumber ?? 1));
      return;
    }
    process.exit(code ?? 1);
  } catch (err) {
    process.stderr.write(`local-rag: could not run ${located.path}: ${err.message}\n`);
    process.exit(1);
  }
}

module.exports = {
  INSTALL_SCRIPT,
  isHealable,
  runHookShim,
  runManagedShim,
};
