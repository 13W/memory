#!/usr/bin/env node
"use strict";

// Ships with the plugin (plugin/bin/), NOT with @13w/memory — this file must
// run correctly when @13w/memory is not installed anywhere on disk, which is
// why it never require()s anything under npm/memory/src/*. It resolves an
// **executable by name**, and nothing else: no `node_modules`, no npm package,
// no `npx`, no cache. Spec 13 §2 `[FIXED, ADR-0013]` states the order —
// `LOCAL_RAG_BIN_DIR`, then the entries of `PATH` in order, then a list of
// well-known global-bin directories — and ADR-0013 Decision 3 says why the
// three tiers this replaces are gone: "Nothing consults `node_modules`, and
// nothing runs `npx`."
//
// WHY A THIRD RUNG AT ALL, when `PATH` already exists. A GUI-launched client
// inherits launchd's `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), not the shell's,
// so a perfectly good global install is invisible to it. That single case is
// what the well-known directories recover, and it is why ADR-0013 rejected the
// simpler "let the client resolve the binary itself" design outright.
//
// THE LIST IS THIS FILE'S OWN CHOICE — the specification names the rung and its
// purpose but enumerates nothing, so the reasoning lives here.
//
// Its first entry is derived rather than guessed: this launcher is started as
// `node <this file>`, and for every Node-managed global install a package's bin
// entries are siblings of `node` itself. One line covers nvm, fnm, volta, a
// system Node and a Homebrew Node at once, and on this machine it is the *only*
// directory that holds a `local-rag` command at all — every hard-coded path
// below is empty here. That is D-055's finding carried out ("a single global
// install is the only route to a network-free cold start") without the
// `require.resolve` machinery ADR-0013 retired.
//
// The rest are the global bin directories of the installers ADR-0013 Decision 3
// names — npm under both Homebrew prefixes, `pnpm link --global`, bun, volta —
// plus the XDG convention. They must stay expressible in POSIX `sh` built-ins,
// because T22-14 requires the hook's shell resolver to produce a byte-identical
// ordered list from the same environment; anything needing `brew --prefix` or a
// config read would make that impossible.
//
// Hard rule, unchanged: every diagnostic line goes to stderr, never stdout.
// Once a child is spawned with stdio:'inherit', this process's stdout *is* the
// MCP JSON-RPC channel, and a stray write corrupts the protocol framing
// (spec 13 §2: "stdout stays byte-empty").

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { constants } = require("node:os");

const FORWARDED_SIGNALS = Object.freeze(["SIGINT", "SIGTERM"]);

const BIN_DIR_VAR = "LOCAL_RAG_BIN_DIR";
const TEST_DIRS_VAR = "LOCAL_RAG_TEST_BIN_DIRS";
const DEBUG_VAR = "LOCAL_RAG_DEBUG";

const SERVER_BINARY = "local-rag-proxy";
// The daemon the proxy will look for beside itself. Spec 13 §4
// `[SPEC, ADR-0013]` makes that co-location a requirement rather than a
// coincidence, and `crates/local-rag-proxy/src/connect.rs:55` is the code that
// depends on it.
const DAEMON_BINARY = "local-rag";

/** An empty variable is unset, the same rule the rest of the project follows. */
function nonemptyVar(env, name) {
  const value = env[name];
  return typeof value === "string" && value !== "" ? value : null;
}

/**
 * Every directory this launcher will look in, in order, without touching the
 * filesystem once.
 *
 * Pure so the other platform's shape can be asserted from this one — the same
 * discipline `npm/memory/src/paths.js` follows, and the same trap D-055
 * recorded: `path.win32`/`path.posix` are named explicitly, because the ambient
 * `path` module joins with the host's separator and a cross-platform test would
 * then pass for the wrong reason.
 *
 * @param {{env?: NodeJS.ProcessEnv, platform?: string, execPath?: string}} [opts]
 * @returns {string[]}
 */
function candidateBinDirs(opts = {}) {
  const env = opts.env ?? process.env;
  const platform = opts.platform ?? process.platform;
  const execPath = opts.execPath ?? process.execPath;

  // The whole list, replaced — not extended. A seam that prepended would let
  // the developer's real PATH decide a test's outcome, and "it resolved" would
  // stop meaning "it resolved where the test put it".
  const replacement = nonemptyVar(env, TEST_DIRS_VAR);
  if (replacement !== null) {
    return dedupe(replacement.split(path.delimiter).filter((d) => d !== ""));
  }

  const p = platform === "win32" ? path.win32 : path.posix;
  const dirs = [];

  const override = nonemptyVar(env, BIN_DIR_VAR);
  if (override !== null) dirs.push(override);

  const pathVar = nonemptyVar(env, "PATH") ?? "";
  const delimiter = platform === "win32" ? ";" : ":";
  for (const entry of pathVar.split(delimiter)) {
    if (entry !== "") dirs.push(entry);
  }

  // Derived, not guessed: a Node-managed global install puts a package's bins
  // beside `node` itself.
  dirs.push(p.dirname(execPath));

  const home = nonemptyVar(env, platform === "win32" ? "USERPROFILE" : "HOME");
  if (platform === "win32") {
    const appData = nonemptyVar(env, "APPDATA");
    if (appData !== null) dirs.push(p.join(appData, "npm"));
    const localAppData = nonemptyVar(env, "LOCALAPPDATA");
    if (localAppData !== null) dirs.push(p.join(localAppData, "pnpm"));
  } else {
    dirs.push("/opt/homebrew/bin", "/usr/local/bin");
    if (home !== null) {
      dirs.push(
        p.join(home, ".local", "bin"),
        p.join(home, ".local", "share", "pnpm"),
        p.join(home, ".bun", "bin"),
        p.join(home, ".volta", "bin"),
        p.join(home, ".npm-global", "bin"),
      );
    }
  }

  return dedupe(dirs);
}

/** Order-preserving, because the order is the contract. */
function dedupe(dirs) {
  const seen = new Set();
  const out = [];
  for (const dir of dirs) {
    if (seen.has(dir)) continue;
    seen.add(dir);
    out.push(dir);
  }
  return out;
}

/** Synchronous, cross-platform-best-effort "is this a runnable file" check. */
function isExecutableFile(p) {
  try {
    fs.accessSync(p, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * The first candidate directory that holds a usable installation.
 *
 * "Usable" means the daemon is there too. Spec 13 §4 requires the daemon to sit
 * beside the proxy that spawns it, and `connect.rs:55` is what relies on it;
 * accepting a directory with only the proxy would move the failure to the
 * moment the proxy looks for its daemon, where it reads as "daemon missing"
 * rather than "the install is incomplete".
 *
 * Note which parts are parameterised and which are not. The *list* is a pure
 * formula and takes `platform`, so the win32 shape can be asserted from a POSIX
 * host. The *probe* joins with the host's own separator — these are files on
 * this machine, and a `\\` in the middle of a POSIX path would look for
 * something that cannot exist. Only the `.exe` suffix follows the injected
 * platform, because that is the one part of the probe that is a naming rule
 * rather than a filesystem fact.
 *
 * @param {string} name
 * @param {object} [opts] see `candidateBinDirs`
 * @returns {{path: string, dir: string} | null}
 */
function resolveBinary(name, opts = {}) {
  const platform = opts.platform ?? process.platform;
  const suffix = platform === "win32" ? ".exe" : "";
  for (const dir of candidateBinDirs(opts)) {
    const candidate = path.join(dir, name + suffix);
    if (!isExecutableFile(candidate)) continue;
    if (name !== DAEMON_BINARY && !isExecutableFile(path.join(dir, DAEMON_BINARY + suffix))) {
      continue;
    }
    return { path: candidate, dir };
  }
  return null;
}

/**
 * Spawn `execPath` as an attached (never detached), stdio-inherited child,
 * forward SIGINT/SIGTERM 1:1, and exit this process with the child's own
 * exit code/signal once it exits. Mirrors npm/memory/src/lifecycle.js's
 * `runAndForwardSignals` — a smaller inline copy, because this file cannot
 * require() that module and must keep working when it is not installed.
 *
 * Once this function is entered there is no further fallback, by design: the
 * resolver has already established synchronously that the file exists and is
 * executable, so an async spawn error is fatal for *this* run rather than a
 * signal to look somewhere else.
 *
 * The body below is preserved verbatim from the three-tier version this file
 * replaces — it is `[FIXED list]` item 1 (signal forwarding, reliable
 * termination, orphan cleanup), and T22-12's card says so.
 *
 * @param {string} execPath
 * @param {string[]} args
 */
function runChildAndExit(execPath, args) {
  const child = spawn(execPath, args, { stdio: "inherit" });
  let shuttingDown = false;
  const handlers = new Map();

  function cleanup() {
    for (const [signal, handler] of handlers) {
      process.off(signal, handler);
    }
    handlers.clear();
  }

  for (const signal of FORWARDED_SIGNALS) {
    const handler = () => {
      if (shuttingDown) {
        return;
      }
      shuttingDown = true;
      try {
        child.kill(signal);
      } catch {
        // The child may already be gone — the 'exit' handler below is the
        // only place that decides this process's own outcome.
      }
    };
    handlers.set(signal, handler);
    process.on(signal, handler);
  }

  child.on("error", (err) => {
    cleanup();
    process.stderr.write(`local-rag: could not run the native binary: ${err.message}\n`);
    process.exit(1);
  });

  child.on("exit", (code, signal) => {
    cleanup();
    if (signal) {
      const signalNumber = constants.signals[signal];
      process.stderr.write(`local-rag: the native binary was terminated by ${signal}\n`);
      process.exit(128 + (signalNumber ?? 1));
      return;
    }
    process.exit(code ?? 1);
  });
}

/**
 * The not-installed contract, spec 13 §2 `[FIXED, ADR-0013]`, verbatim in
 * behaviour: stdout stays byte-empty because it is the JSON-RPC stream, the
 * diagnostic names both the install command and the override variable, and the
 * process exits non-zero so the client shows a failed server rather than a
 * silent one.
 *
 * The candidate list is printed only under `LOCAL_RAG_DEBUG=1`. It is the right
 * answer to "why did it not find it" and the wrong thing to put in front of
 * somebody who just wants the one command that fixes this.
 */
function reportNotInstalled(env) {
  process.stderr.write(
    "local-rag: the memory server is not installed.\n" +
      "The plugin never downloads anything — obtaining the binaries is the npm\n" +
      `package's job. Set ${BIN_DIR_VAR} to a directory of prebuilt binaries for\n` +
      "an offline or air-gapped install.\n" +
      "Fix:\n" +
      "  npm i -g @13w/memory\n",
  );
  if (nonemptyVar(env, DEBUG_VAR) !== null) {
    process.stderr.write(`local-rag: looked in (in order):\n`);
    for (const dir of candidateBinDirs({ env })) {
      process.stderr.write(`  ${dir}\n`);
    }
  }
}

function main() {
  const resolved = resolveBinary(SERVER_BINARY);
  if (resolved === null) {
    reportNotInstalled(process.env);
    process.exit(1);
    return;
  }
  runChildAndExit(resolved.path, process.argv.slice(2));
}

if (require.main === module) {
  main();
}

// Exported for plugin/test/mcp-launcher-resolution.test.js only — production
// entry is the `require.main === module` branch above.
module.exports = {
  BIN_DIR_VAR,
  TEST_DIRS_VAR,
  DEBUG_VAR,
  SERVER_BINARY,
  DAEMON_BINARY,
  candidateBinDirs,
  isExecutableFile,
  resolveBinary,
  main,
};
