#!/usr/bin/env node
"use strict";

// Ships with the plugin (plugin/bin/), NOT with @13w/memory — this file
// must run correctly even when @13w/memory is not installed anywhere on
// disk (that is the whole reason tier 3 exists), so it never require()s
// anything under npm/memory/src/*. Every helper below is a small,
// deliberate, commented duplication of a convention that also lives in
// npm/memory/src/{platform,binary-cache}.js — kept honest by
// plugin/test/mcp-launcher-tiers.test.js, which CAN require both trees
// and assert they agree.
//
// Three tiers, tried in order (T19-03, group 19 plan — replaces the bare
// `npx --yes --package=@13w/memory local-rag-mcp` `.mcp.json` used to run
// directly):
//   1. @13w/memory installed on this machine (require()-delegated to its
//      own bin/local-rag-mcp.js — zero duplicated resolution logic, zero
//      extra nested Node process; see tier1()'s own doc comment for why a
//      bare require() is not safe on its own). Two anchors are tried, in
//      order: the current project's own node_modules (an explicit local
//      override/pin, checked first so it wins when present) and this
//      machine's global npm install location (D-055: the intended common
//      case — the plugin ships through the marketplace, which never
//      installs the server itself, so a real user's only path to a
//      network-free cold start is `npm install --global @13w/memory` once,
//      usable from every project; a plain project-local check alone can
//      never see that).
//   2. the known-path cache at ${CLAUDE_PLUGIN_DATA}/bin/local-rag-proxy,
//      populated by a previous successful tier-1 or tier-3 run (both
//      paths run npm/memory/bin/local-rag-mcp.js under the hood, which
//      refreshes this cache as a side effect — no write-path code needed
//      here, only the read/stat check)
//   3. `npx --yes --package=@13w/memory local-rag-mcp`, today's only
//      behavior, kept as the universally-correct last resort
//
// Hard rule: every diagnostic line below goes to stderr, never stdout.
// Once any tier's child is spawned with stdio:'inherit', this process's
// stdout *is* the MCP JSON-RPC channel — a stray stdout write corrupts
// the protocol framing.

const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { constants } = require("node:os");

const FORWARDED_SIGNALS = Object.freeze(["SIGINT", "SIGTERM"]);

/** Mirrors npm/memory/src/platform.js's platformKey()+platformPackageName(). */
function platformPackageName() {
  return `@13w/memory-${process.platform}-${process.arch}`;
}

/**
 * Where a plain `npm install --global @13w/memory` lands, computed
 * synchronously from `process.execPath` — the same default-prefix
 * convention npm itself falls back to absent a custom `.npmrc` prefix
 * (`<node-install>/lib/node_modules` on POSIX, `<node-install>/node_modules`
 * on win32, since Windows npm's default prefix is the Node install
 * directory itself, not a `lib/` subdirectory of it). Deliberately not
 * `npm root -g`/`npm config get prefix` (a Node/npm subprocess spawn,
 * historically 100-300ms — too expensive for what is supposed to be the
 * fast tier): a user who has customized their global prefix away from this
 * default is a known, accepted miss here, same trade-off as before — they
 * fall through to tier 2/tier 3, not a crash or a wrong answer.
 *
 * @param {string} [execPath] defaults to `process.execPath`; a parameter
 *   only so this stays a pure, directly unit-testable function.
 * @param {string} [platform] defaults to `process.platform`.
 * @returns {string}
 */
function npmGlobalNodeModules(execPath = process.execPath, platform = process.platform) {
  // path.win32/path.posix, not the ambient `path` (host-OS-flavored): a
  // test on a POSIX CI runner must still be able to verify the win32
  // shape (backslash-separated) correctly, and vice versa — real
  // production calls only ever pair a real `process.execPath` with the
  // matching real `process.platform`, so this is a no-op switch there.
  const p = platform === "win32" ? path.win32 : path.posix;
  const nodeDir = p.dirname(execPath);
  return platform === "win32"
    ? p.join(nodeDir, "node_modules")
    : p.join(p.dirname(nodeDir), "lib", "node_modules");
}

/**
 * The anchor tier1() actually uses for "installed globally on this
 * machine". `LOCAL_RAG_TEST_GLOBAL_NODE_MODULES` is a test-only override,
 * unset (and inert) in every real invocation — this machine's *real*
 * global npm directory always lives under the user's home directory
 * (`~/.nvm/...`, `~/.local/...`, platform-package-manager-specific), and
 * CLAUDE.md forbids tests depending on it; this seam is what lets
 * `plugin/test/mcp-launcher-tiers.test.js` exercise tier1()'s global path
 * against a hermetic fixture instead (same class of test-only,
 * zero-production-effect override as the Rust side's
 * `LOCAL_RAG_TEST_FAKE_DAEMON_VERSION`).
 *
 * @returns {string}
 */
function globalNodeModulesAnchor() {
  return process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES || npmGlobalNodeModules();
}

/** Mirrors npm/memory/src/binary-cache.js's cachedBinaryPath(..., "local-rag-proxy"). */
function cachedProxyPath() {
  const pluginData = process.env.CLAUDE_PLUGIN_DATA;
  if (!pluginData) {
    return null;
  }
  const suffix = process.platform === "win32" ? ".exe" : "";
  return path.join(pluginData, "bin", "local-rag-proxy" + suffix);
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
 * Spawn `execPath` as an attached (never detached), stdio-inherited child,
 * forward SIGINT/SIGTERM 1:1, and exit this process with the child's own
 * exit code/signal once it exits. Mirrors npm/memory/src/lifecycle.js's
 * `runAndForwardSignals` + npm/memory/bin/local-rag-mcp.js's own exit-code
 * mapping — a smaller inline copy (this file cannot require() that
 * module), used identically by tier2/tier3 below. Once a tier commits to
 * spawning here there is no further fallback, by design: this is the same
 * point of no return `local-rag-mcp.js`'s own `runAndForwardSignals(...)
 * .catch(...)` already is for the standalone-binary case — an async spawn
 * error (rare once tier2's own synchronous existence/executable-bit check
 * already passed) is fatal for *this* run, not a signal to try the next
 * tier.
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
 * Tier 1: `@13w/memory` installed on this machine. Two anchors are tried,
 * in order (D-055 — the original single-anchor, project-only design
 * turned out to make the intended common case, a real user's global
 * install, unreachable; see the module doc comment above and
 * `DEVIATIONS.md` D-055 for the full history):
 *
 *   1. `${CLAUDE_PROJECT_DIR}/node_modules` — an explicit local
 *      override/pin (monorepo vendoring, a pinned version for
 *      reproducibility); checked first so it wins when present, the same
 *      "local beats global" precedence most npm-based CLI tools already
 *      give a project devDependency over a global install.
 *   2. `npmGlobalNodeModules()` — this machine's global npm install
 *      location (`globalNodeModulesAnchor()`, test-overridable). Not a
 *      monorepo-relative guess off `${CLAUDE_PLUGIN_ROOT}` either way (a
 *      real, marketplace-installed `${CLAUDE_PLUGIN_ROOT}` points into
 *      `~/.claude/plugins/cache/...`, unrelated to any project's own
 *      source layout, so guessing a relative path from there would be
 *      actively wrong for every real user, not just unnecessary).
 *
 * Delegates to the already-correct `@13w/memory/bin/local-rag-mcp.js` via
 * `require()` rather than duplicating its resolution logic, and rather
 * than `spawn`-ing it (which would pay a second, nested Node bootstrap on
 * top of this one). That file's own `main()` calls `process.exit(1)`
 * synchronously on a resolution failure — fatal by design for the
 * standalone-binary case — so a bare `require()` here could kill this
 * whole launcher before tier 2/3 ever run. The preflight below
 * independently confirms *both* the base package and the
 * platform-specific package resolve from the same anchor before
 * committing to `require()`, so the common real-world failure mode (the
 * platform `optionalDependency` was skipped — `npm install
 * --omit=optional`, a lockfile mismatch, a registry hiccup) is caught
 * here and falls through cleanly, never fatal. Residual, accepted risk:
 * if the preflight passes but the delegated module's own resolution still
 * somehow disagrees, or its resolved binary fails to spawn asynchronously
 * after `require()` returns, its `main()` calls `process.exit()` directly
 * and this function cannot intercept that — narrow, since preflight and
 * delegate use the identical anchor and the same deterministic
 * `require.resolve` algorithm.
 *
 * @returns {boolean} true if this tier took over (in practice `require()`
 *   already called `process.exit()` by the time this returns).
 */
function tier1() {
  const projectDir = process.env.CLAUDE_PROJECT_DIR;
  const anchors = [];
  if (projectDir) {
    anchors.push(path.join(projectDir, "node_modules"));
  }
  anchors.push(globalNodeModulesAnchor());
  try {
    require.resolve(`${platformPackageName()}/package.json`, { paths: anchors });
  } catch {
    // Routine, expected miss for the vast majority of real users (no
    // local @13w/memory install at all) — silent by design, not a bug.
    // Only a failure *after* both preflight checks already passed (the
    // require() below) is surprising enough to be worth a stderr note.
    return false;
  }
  let launcherPath;
  try {
    launcherPath = require.resolve("@13w/memory/bin/local-rag-mcp.js", { paths: anchors });
  } catch {
    return false; // same routine-miss reasoning as above
  }
  try {
    require(launcherPath); // self-executing: handles its own stdio/signals/exit,
    return true; // and (via binary-cache.js) refreshes tier 2's cache too
  } catch (err) {
    process.stderr.write(
      `local-rag: installed @13w/memory failed to start, falling back: ${err.message}\n`,
    );
    return false;
  }
}

/**
 * Tier 2: the cache tier 1/tier 3 populate on any prior successful start.
 * A missing or non-executable cache entry falls through to tier 3
 * synchronously, before ever spawning — see `runChildAndExit`'s own doc
 * comment for why a spawn failure *after* this check is not itself a
 * fallback trigger.
 *
 * @returns {boolean}
 */
function tier2() {
  const cached = cachedProxyPath();
  if (!cached || !isExecutableFile(cached)) {
    return false;
  }
  runChildAndExit(cached, process.argv.slice(2));
  return true;
}

/** Tier 3: today's only behavior, unconditional last resort. */
function tier3() {
  runChildAndExit("npx", [
    "--yes",
    "--package=@13w/memory",
    "local-rag-mcp",
    ...process.argv.slice(2),
  ]);
}

function main() {
  if (tier1()) {
    return;
  }
  if (tier2()) {
    return;
  }
  tier3();
}

if (require.main === module) {
  main();
}

// Exported for plugin/test/mcp-launcher-tiers.test.js only — production
// entry is the `require.main === module` branch above.
module.exports = {
  main,
  tier1,
  tier2,
  tier3,
  platformPackageName,
  cachedProxyPath,
  npmGlobalNodeModules,
};
