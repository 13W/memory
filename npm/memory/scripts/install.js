#!/usr/bin/env node
"use strict";

// `local-rag`'s binary installer as a command (T22-08, ADR-0013).
//
// Two callers, two modes, and the difference is not cosmetic. ADR-0013
// Decision 2 makes "update the package, get the new services" true by having
// `postinstall` re-resolve `latest` and refetch when the tag moved — so the
// default mode always asks. But the lazy repair path, reached when a shim
// finds no binary, runs on a cold start under a fifty-millisecond hook budget
// and must not touch the network merely to confirm what the manifest already
// says. That is `--if-needed`.
//
// This command exits non-zero when an install fails, because an installer that
// reports success it did not achieve cannot be debugged afterwards. Deciding
// not to fail `npm install` over it belongs to `scripts/postinstall.js`
// (T22-10), which always exits 0 — the two are deliberately separate.

const path = require("node:path");
const { spawn } = require("node:child_process");

const { platformKey } = require("../src/platform");
const { installDir, PathsError } = require("../src/paths");
const { installBinaries, ERROR_FILE, InstallError } = require("../src/install");
const { LockError } = require("../src/lock");

const USAGE = `Usage: node <package>/scripts/install.js [options]

  --dir <path>     install into <path> instead of the per-user cache
  --if-needed      do nothing, and touch no network, when already installed
  --force          reinstall even when the manifest says it is current
  --background     detach and return immediately; this process exits 0
  --no-wait        fail at once when another install holds the lock
  --help           print this

Exit codes: 0 installed or already current, 1 failed, 2 bad usage.
`;

function parseArgs(argv) {
  const opts = { dir: null, mode: "update", background: false, wait: true };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--help" || arg === "-h") return { help: true };
    else if (arg === "--if-needed") opts.mode = "if-needed";
    else if (arg === "--force") opts.mode = "force";
    else if (arg === "--background") opts.background = true;
    else if (arg === "--no-wait") opts.wait = false;
    else if (arg === "--dir") {
      i += 1;
      if (i >= argv.length) return { error: "--dir needs a path" };
      opts.dir = argv[i];
    } else if (arg.startsWith("--dir=")) opts.dir = arg.slice("--dir=".length);
    else return { error: `unknown option: ${arg}` };
  }
  return opts;
}

/**
 * Re-run this script detached, mirroring
 * `crates/local-rag-proxy/src/connect.rs::spawn_detached_daemon`: its own
 * process group so a Ctrl-C in the parent's terminal does not reach it, and no
 * inherited stdio at all. Every other spawn in this package is deliberately
 * attached (`src/lifecycle.js`) — this is the one place that must outlive its
 * caller.
 */
function relaunchDetached(argv) {
  const args = [__filename, ...argv.filter((a) => a !== "--background")];
  const child = spawn(process.execPath, args, {
    detached: true,
    stdio: "ignore",
  });
  child.unref();
}

async function main() {
  const parsed = parseArgs(process.argv.slice(2));
  if (parsed.help) {
    process.stdout.write(USAGE);
    return 0;
  }
  if (parsed.error) {
    process.stderr.write(`local-rag: ${parsed.error}\n\n${USAGE}`);
    return 2;
  }

  if (parsed.background) {
    relaunchDetached(process.argv.slice(2));
    return 0;
  }

  const key = platformKey();
  let dir = parsed.dir;
  // The private-store default gets the store's 0700; a caller-named directory
  // gets whatever policy already applies there. `install.js` refuses to guess.
  let dirMode;
  if (dir === null) {
    try {
      dir = installDir(key);
      dirMode = 0o700;
    } catch (err) {
      if (err instanceof PathsError) {
        process.stderr.write(`local-rag: ${err.message}\n`);
        return 1;
      }
      throw err;
    }
  }

  try {
    const report = await installBinaries(dir, {
      mode: parsed.mode,
      dirMode,
      log: (line) => process.stdout.write(`${line}\n`),
      lock: {
        wait: parsed.wait,
        // Progress a user waiting on `npm install` should be told about, and
        // the handshake the concurrency test synchronises on rather than
        // guessing with a timer.
        onEvent: (event) => process.stdout.write(`LOCK ${event}\n`),
      },
    });
    if (report.skipped) {
      process.stdout.write(`local-rag: ${report.reason} (${report.tag ?? "no tag"})\n`);
    } else {
      process.stdout.write(`local-rag: installed ${report.tag} into ${dir}\n`);
      if (report.absent.length > 0) {
        process.stdout.write(`local-rag: not in this release: ${report.absent.join(", ")}\n`);
      }
    }
    return 0;
  } catch (err) {
    if (err instanceof LockError && err.kind === "held") {
      const owner = err.owner;
      const who = owner && owner.pid ? ` held by pid ${owner.pid} on ${owner.hostname}` : "";
      process.stderr.write(`local-rag: another install is running${who}\n`);
      return 1;
    }
    if (err instanceof InstallError || err instanceof LockError || err instanceof Error) {
      process.stderr.write(`${err.message}\n`);
      const detail = path.join(dir, ERROR_FILE);
      process.stderr.write(`local-rag: details were also written to ${detail}\n`);
      return 1;
    }
    throw err;
  }
}

main().then(
  (code) => process.exit(code),
  (err) => {
    process.stderr.write(`local-rag: ${err && err.stack ? err.stack : err}\n`);
    process.exit(1);
  },
);
