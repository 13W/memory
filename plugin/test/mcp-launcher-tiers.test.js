"use strict";

// T19-03/D-055: behavioral tests for plugin/bin/local-rag-mcp-launcher.js's
// three-tier dispatch (@13w/memory installed on this machine -> known-path
// cache -> npx last resort). Two styles, deliberately:
//
//   - "Group A" below calls tier1()/tier2() directly via require() — safe
//     ONLY for their fall-through (`return false`) branches, which never
//     spawn anything. Their success branches call `runChildAndExit`,
//     which ends in `process.exit()` — calling those in-process would
//     kill this test runner, so success paths are never exercised this
//     way (see the launcher's own doc comments for why `require.main ===
//     module` gates its `main()` call).
//   - "Group B" spawns the real launcher as a subprocess (mirrors
//     `npm/memory/test/subprocess.test.js`'s own philosophy: genuine
//     child processes, no mocking) and observes stdout/exit behavior —
//     the only safe way to exercise a tier's success path end to end.
//
// D-055: tier1() now tries two anchors (project-local, then a machine-
// global npm install). Every test below that does not itself want the
// global anchor to resolve pins `LOCAL_RAG_TEST_GLOBAL_NODE_MODULES` to a
// guaranteed-empty directory — this machine's *real* global npm modules
// dir lives under the user's home directory, and CLAUDE.md forbids tests
// depending on it, so a test must never rely on that real location being
// empty by chance.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

const { platformKey, platformPackageName: realPlatformPackageName } = require("../../npm/memory/src/platform.js");
const { cachedBinaryPath } = require("../../npm/memory/src/binary-cache.js");
const { buildFlatLayout } = require("../../npm/memory/test/helpers/fixture-layout.js");
const { mkTmpRoot } = require("../../npm/memory/test/helpers/tmp.js");
const { waitForStdoutLine, waitUntil, pidIsAlive } = require("../../npm/memory/test/helpers/proc.js");

const LAUNCHER_FILE = path.join(__dirname, "..", "bin", "local-rag-mcp-launcher.js");
const FAKE_BINARY_SRC = fs.readFileSync(
  path.join(__dirname, "..", "..", "npm", "memory", "test", "helpers", "fake-binary.js"),
  "utf8",
);
const FAKE_NPX_SRC = fs.readFileSync(path.join(__dirname, "helpers", "fake-npx.js"), "utf8");

const HOST_KEY = platformKey();
const HOST_PACKAGE_NAME = `@13w/memory-${HOST_KEY}`;

function withoutEnvKeys(env, keys) {
  const copy = { ...env };
  for (const k of keys) {
    delete copy[k];
  }
  return copy;
}

/**
 * A `LOCAL_RAG_TEST_GLOBAL_NODE_MODULES` value guaranteed to resolve
 * nothing — a fresh, empty temp dir, not a nonexistent path (avoids any
 * platform-specific ENOENT-vs-ENOTDIR difference in how `require.resolve`
 * reports a totally absent ancestor). Caller owns cleanup via the returned
 * path, same convention every other `mkTmpRoot()` call site in this file
 * already follows.
 *
 * @returns {string}
 */
function noGlobalInstallDir() {
  return mkTmpRoot("lr-launcher-noglobal-");
}

/** A `local-rag-proxy` stand-in that fails loudly if ever invoked — used to prove a tier was never reached, not just that this tier's own output looks right. */
const POISON_BINARY_SRC =
  '#!/usr/bin/env node\nprocess.stderr.write("WRONG_TIER_INVOKED\\n");\nprocess.exit(1);\n';

// Group B below is POSIX-only (see the skip reason on each test) — this
// helper is never called on win32, so it stays a plain shebang script,
// the same convention `fake-binary.js` itself already uses unconditionally.
/** A PATH with a fresh fake `npx` prepended. */
function pathWithFakeNpx(npxDir) {
  const npxFile = path.join(npxDir, "npx");
  fs.writeFileSync(npxFile, FAKE_NPX_SRC);
  fs.chmodSync(npxFile, 0o755);
  return `${npxDir}${path.delimiter}${process.env.PATH}`;
}

/** A PATH deliberately excluding any directory that could contain a real npx. */
function pathWithoutNpx() {
  return path.dirname(process.execPath);
}

function spawnLauncher(env) {
  return spawn(process.execPath, [LAUNCHER_FILE], { stdio: ["ignore", "pipe", "pipe"], env });
}

function collectText(stream) {
  const chunks = [];
  stream.on("data", (c) => chunks.push(c));
  return () => Buffer.concat(chunks).toString("utf8");
}

async function expectReadyThenStop(launcher) {
  const { lines } = await waitForStdoutLine(launcher, (l) => l.startsWith("READY "));
  const match = /^READY pid=(\d+)/.exec(lines.find((l) => l.startsWith("READY ")));
  assert.ok(match, `expected a READY line, got: ${JSON.stringify(lines)}`);
  const childPid = Number(match[1]);
  process.kill(launcher.pid, "SIGTERM");
  await new Promise((resolve) => launcher.on("exit", resolve));
  await waitUntil(() => !pidIsAlive(childPid), { description: "grandchild to exit after cleanup" });
  return lines;
}

// ---------------------------------------------------------------------
// Group A — pure fall-through logic, safe to call in-process via require()
// ---------------------------------------------------------------------

test("platformPackageName() matches the real platform.js convention", () => {
  delete require.cache[LAUNCHER_FILE];
  const { platformPackageName } = require(LAUNCHER_FILE);
  assert.equal(platformPackageName(), realPlatformPackageName(HOST_KEY));
});

test("cachedProxyPath() matches the real binary-cache.js convention", () => {
  delete require.cache[LAUNCHER_FILE];
  const { cachedProxyPath } = require(LAUNCHER_FILE);
  const root = mkTmpRoot("lr-launcher-cachepath-");
  const saved = process.env.CLAUDE_PLUGIN_DATA;
  process.env.CLAUDE_PLUGIN_DATA = root;
  try {
    assert.equal(cachedProxyPath(), cachedBinaryPath(root, "local-rag-proxy"));
  } finally {
    if (saved === undefined) {
      delete process.env.CLAUDE_PLUGIN_DATA;
    } else {
      process.env.CLAUDE_PLUGIN_DATA = saved;
    }
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("npmGlobalNodeModules() computes npm's own default-prefix convention, per platform", () => {
  delete require.cache[LAUNCHER_FILE];
  const { npmGlobalNodeModules } = require(LAUNCHER_FILE);
  assert.equal(
    npmGlobalNodeModules("/usr/local/bin/node", "darwin"),
    "/usr/local/lib/node_modules",
  );
  assert.equal(
    npmGlobalNodeModules("/home/zero/.nvm/versions/node/v24.0.0/bin/node", "linux"),
    "/home/zero/.nvm/versions/node/v24.0.0/lib/node_modules",
  );
  assert.equal(
    npmGlobalNodeModules("C:\\Program Files\\nodejs\\node.exe", "win32"),
    "C:\\Program Files\\nodejs\\node_modules",
  );
});

test("tier1() returns false, does not throw, when CLAUDE_PROJECT_DIR is unset and no global install exists", () => {
  delete require.cache[LAUNCHER_FILE];
  const { tier1 } = require(LAUNCHER_FILE);
  const noGlobal = noGlobalInstallDir();
  const savedProjectDir = process.env.CLAUDE_PROJECT_DIR;
  const savedGlobal = process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES;
  delete process.env.CLAUDE_PROJECT_DIR;
  process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = noGlobal;
  try {
    assert.equal(tier1(), false);
  } finally {
    if (savedProjectDir !== undefined) {
      process.env.CLAUDE_PROJECT_DIR = savedProjectDir;
    }
    if (savedGlobal === undefined) {
      delete process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES;
    } else {
      process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = savedGlobal;
    }
    fs.rmSync(noGlobal, { recursive: true, force: true });
  }
});

test("tier1() falls through cleanly (regression: does not process.exit) when the platform package is missing but the base package resolves", () => {
  // This is the exact real-world failure mode the launcher's own doc
  // comment names (`npm install --omit=optional` skipped the platform
  // optionalDependency): the base @13w/memory package IS installed, its
  // own bin/local-rag-mcp.js exists, but no platform-specific package
  // sits alongside it. A naive `require()` of that file would hit its
  // internal `resolvePlatformPackage` failure and call `process.exit(1)`
  // — this test proves the preflight check catches it first.
  const root = mkTmpRoot("lr-launcher-partial-");
  buildFlatLayout(root, []); // launcher package only, zero platform packages
  const noGlobal = noGlobalInstallDir();
  delete require.cache[LAUNCHER_FILE];
  const { tier1 } = require(LAUNCHER_FILE);
  const savedProjectDir = process.env.CLAUDE_PROJECT_DIR;
  const savedGlobal = process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES;
  process.env.CLAUDE_PROJECT_DIR = root;
  process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = noGlobal;
  try {
    assert.equal(tier1(), false, "must fall through, not exit the test process");
  } finally {
    if (savedProjectDir === undefined) {
      delete process.env.CLAUDE_PROJECT_DIR;
    } else {
      process.env.CLAUDE_PROJECT_DIR = savedProjectDir;
    }
    if (savedGlobal === undefined) {
      delete process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES;
    } else {
      process.env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = savedGlobal;
    }
    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(noGlobal, { recursive: true, force: true });
  }
});

test("tier2() returns false when CLAUDE_PLUGIN_DATA is unset", () => {
  delete require.cache[LAUNCHER_FILE];
  const { tier2 } = require(LAUNCHER_FILE);
  const saved = process.env.CLAUDE_PLUGIN_DATA;
  delete process.env.CLAUDE_PLUGIN_DATA;
  try {
    assert.equal(tier2(), false);
  } finally {
    if (saved !== undefined) {
      process.env.CLAUDE_PLUGIN_DATA = saved;
    }
  }
});

test("tier2() returns false when nothing has ever populated the cache", () => {
  const root = mkTmpRoot("lr-launcher-nocache-");
  delete require.cache[LAUNCHER_FILE];
  const { tier2 } = require(LAUNCHER_FILE);
  const saved = process.env.CLAUDE_PLUGIN_DATA;
  process.env.CLAUDE_PLUGIN_DATA = root;
  try {
    assert.equal(tier2(), false);
  } finally {
    if (saved === undefined) {
      delete process.env.CLAUDE_PLUGIN_DATA;
    } else {
      process.env.CLAUDE_PLUGIN_DATA = saved;
    }
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("tier2() returns false for a stale cache entry (symlink target deleted)", () => {
  const root = mkTmpRoot("lr-launcher-stale-");
  const binDir = path.join(root, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const suffix = process.platform === "win32" ? ".exe" : "";
  fs.symlinkSync(path.join(root, "gone" + suffix), path.join(binDir, "local-rag-proxy" + suffix));

  delete require.cache[LAUNCHER_FILE];
  const { tier2 } = require(LAUNCHER_FILE);
  const saved = process.env.CLAUDE_PLUGIN_DATA;
  process.env.CLAUDE_PLUGIN_DATA = root;
  try {
    assert.equal(tier2(), false);
  } finally {
    if (saved === undefined) {
      delete process.env.CLAUDE_PLUGIN_DATA;
    } else {
      process.env.CLAUDE_PLUGIN_DATA = saved;
    }
    fs.rmSync(root, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------
// Group B — real subprocess, end to end. POSIX-only (SIGTERM-based
// cleanup), same gate `npm/memory/test/subprocess.test.js` already
// established for this exact class of test.
// ---------------------------------------------------------------------

const POSIX_ONLY = {
  skip: process.platform === "win32" && "signal-forwarding tests are POSIX-only (see lifecycle.js's Windows note)",
};

test("tier 1 hit: a project-local @13w/memory starts the server, npx never invoked", POSIX_ONLY, async () => {
  const root = mkTmpRoot("lr-launcher-t1-");
  buildFlatLayout(root, [
    {
      name: HOST_PACKAGE_NAME,
      platform: process.platform,
      cpu: process.arch,
      binaryContents: { "local-rag-proxy": FAKE_BINARY_SRC },
    },
  ]);
  const noGlobal = noGlobalInstallDir();

  const env = withoutEnvKeys(process.env, ["CLAUDE_PLUGIN_DATA"]);
  env.CLAUDE_PROJECT_DIR = root;
  env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = noGlobal;
  const launcher = spawnLauncher(env);
  const getStderr = collectText(launcher.stderr);

  const lines = await expectReadyThenStop(launcher);
  assert.ok(!lines.some((l) => l.startsWith("NPX_ARGS")), `npx must never be invoked: ${getStderr()}`);

  fs.rmSync(root, { recursive: true, force: true });
  fs.rmSync(noGlobal, { recursive: true, force: true });
});

// D-055: the original single-anchor tier1() made a real user's global
// install unreachable — see DEVIATIONS.md D-055. This is the case that
// deviation exists to fix: no project-local install at all, only a
// machine-global one (what `npm install --global @13w/memory` produces).
test("tier 1 hit via a global npm install (no CLAUDE_PROJECT_DIR): starts the server, npx never invoked", POSIX_ONLY, async () => {
  const globalRoot = mkTmpRoot("lr-launcher-t1global-");
  buildFlatLayout(globalRoot, [
    {
      name: HOST_PACKAGE_NAME,
      platform: process.platform,
      cpu: process.arch,
      binaryContents: { "local-rag-proxy": FAKE_BINARY_SRC },
    },
  ]);

  const env = withoutEnvKeys(process.env, ["CLAUDE_PLUGIN_DATA", "CLAUDE_PROJECT_DIR"]);
  env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = path.join(globalRoot, "node_modules");
  const launcher = spawnLauncher(env);
  const getStderr = collectText(launcher.stderr);

  const lines = await expectReadyThenStop(launcher);
  assert.ok(!lines.some((l) => l.startsWith("NPX_ARGS")), `npx must never be invoked: ${getStderr()}`);

  fs.rmSync(globalRoot, { recursive: true, force: true });
});

test("project-local install wins over a global install when both are present", POSIX_ONLY, async () => {
  const projectRoot = mkTmpRoot("lr-launcher-t1precedence-project-");
  buildFlatLayout(projectRoot, [
    {
      name: HOST_PACKAGE_NAME,
      platform: process.platform,
      cpu: process.arch,
      binaryContents: { "local-rag-proxy": FAKE_BINARY_SRC },
    },
  ]);
  const globalRoot = mkTmpRoot("lr-launcher-t1precedence-global-");
  buildFlatLayout(globalRoot, [
    {
      name: HOST_PACKAGE_NAME,
      platform: process.platform,
      cpu: process.arch,
      // A poison binary: if this ever runs instead of the project-local
      // one, the test fails loudly (WRONG_TIER_INVOKED on stderr) rather
      // than passing by accident because both binaries happen to print a
      // READY line.
      binaryContents: { "local-rag-proxy": POISON_BINARY_SRC },
    },
  ]);

  const env = withoutEnvKeys(process.env, ["CLAUDE_PLUGIN_DATA"]);
  env.CLAUDE_PROJECT_DIR = projectRoot;
  env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = path.join(globalRoot, "node_modules");
  const launcher = spawnLauncher(env);
  const getStderr = collectText(launcher.stderr);

  const lines = await expectReadyThenStop(launcher);
  assert.ok(!lines.some((l) => l.startsWith("NPX_ARGS")), `npx must never be invoked: ${getStderr()}`);
  assert.ok(!getStderr().includes("WRONG_TIER_INVOKED"), `the global (poison) binary must never run: ${getStderr()}`);

  fs.rmSync(projectRoot, { recursive: true, force: true });
  fs.rmSync(globalRoot, { recursive: true, force: true });
});

test("tier 1 partial failure falls through to tier 2 end to end (real subprocess)", POSIX_ONLY, async () => {
  const projectRoot = mkTmpRoot("lr-launcher-t1fail-");
  buildFlatLayout(projectRoot, []); // base package only, no platform package
  const noGlobal = noGlobalInstallDir();

  const pluginData = mkTmpRoot("lr-launcher-t1fail-cache-");
  const binDir = path.join(pluginData, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const cacheFile = path.join(binDir, "local-rag-proxy" + (process.platform === "win32" ? ".exe" : ""));
  fs.writeFileSync(cacheFile, FAKE_BINARY_SRC);
  fs.chmodSync(cacheFile, 0o755);

  const env = { ...process.env };
  env.CLAUDE_PROJECT_DIR = projectRoot;
  env.CLAUDE_PLUGIN_DATA = pluginData;
  env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = noGlobal;
  const launcher = spawnLauncher(env);
  const getStderr = collectText(launcher.stderr);

  const lines = await expectReadyThenStop(launcher);
  assert.ok(!lines.some((l) => l.startsWith("NPX_ARGS")), `npx must never be invoked: ${getStderr()}`);
  // A missing platform package is the routine, expected case for the vast
  // majority of real users (who have no local @13w/memory install at
  // all) — tier1() stays silent for it by design, unlike a genuine
  // require() failure after the preflight already passed (see the
  // launcher's own tier1() doc comment). Only that latter, surprising
  // case is worth a stderr note.
  assert.equal(getStderr(), "", "a routine tier-1 miss must not print diagnostic noise");

  fs.rmSync(projectRoot, { recursive: true, force: true });
  fs.rmSync(pluginData, { recursive: true, force: true });
  fs.rmSync(noGlobal, { recursive: true, force: true });
});

test("tier 1 skipped (no CLAUDE_PROJECT_DIR, no global install), tier 2 hit: the cache starts the server", POSIX_ONLY, async () => {
  const pluginData = mkTmpRoot("lr-launcher-t2-");
  const binDir = path.join(pluginData, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const cacheFile = path.join(binDir, "local-rag-proxy" + (process.platform === "win32" ? ".exe" : ""));
  fs.writeFileSync(cacheFile, FAKE_BINARY_SRC);
  fs.chmodSync(cacheFile, 0o755);
  const noGlobal = noGlobalInstallDir();

  const env = withoutEnvKeys(process.env, ["CLAUDE_PROJECT_DIR"]);
  env.CLAUDE_PLUGIN_DATA = pluginData;
  env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = noGlobal;
  const launcher = spawnLauncher(env);
  const getStderr = collectText(launcher.stderr);

  const lines = await expectReadyThenStop(launcher);
  assert.ok(!lines.some((l) => l.startsWith("NPX_ARGS")), `npx must never be invoked: ${getStderr()}`);

  fs.rmSync(pluginData, { recursive: true, force: true });
  fs.rmSync(noGlobal, { recursive: true, force: true });
});

test("tier 2 stale falls through to tier 3, which is invoked with exactly the expected npx arguments", POSIX_ONLY, async () => {
  const pluginData = mkTmpRoot("lr-launcher-t3-");
  const binDir = path.join(pluginData, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const suffix = process.platform === "win32" ? ".exe" : "";
  fs.symlinkSync(path.join(pluginData, "gone" + suffix), path.join(binDir, "local-rag-proxy" + suffix));

  const npxDir = mkTmpRoot("lr-launcher-fakenpx-");
  const noGlobal = noGlobalInstallDir();
  const env = withoutEnvKeys(process.env, ["CLAUDE_PROJECT_DIR"]);
  env.CLAUDE_PLUGIN_DATA = pluginData;
  env.PATH = pathWithFakeNpx(npxDir);
  env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = noGlobal;
  const launcher = spawnLauncher(env);

  const lines = await expectReadyThenStop(launcher);
  const npxLine = lines.find((l) => l.startsWith("NPX_ARGS"));
  assert.ok(npxLine, `tier 3 must be reached: ${JSON.stringify(lines)}`);
  assert.deepEqual(
    JSON.parse(npxLine.slice("NPX_ARGS ".length)),
    ["--yes", "--package=@13w/memory", "local-rag-mcp"],
  );

  fs.rmSync(pluginData, { recursive: true, force: true });
  fs.rmSync(npxDir, { recursive: true, force: true });
  fs.rmSync(noGlobal, { recursive: true, force: true });
});

test("card requirement: registry unreachable (no npx on PATH), local cached binary exists -> server still starts", POSIX_ONLY, async () => {
  const pluginData = mkTmpRoot("lr-launcher-negative-");
  const binDir = path.join(pluginData, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const cacheFile = path.join(binDir, "local-rag-proxy" + (process.platform === "win32" ? ".exe" : ""));
  fs.writeFileSync(cacheFile, FAKE_BINARY_SRC);
  fs.chmodSync(cacheFile, 0o755);
  const noGlobal = noGlobalInstallDir();

  const env = withoutEnvKeys(process.env, ["CLAUDE_PROJECT_DIR"]);
  env.CLAUDE_PLUGIN_DATA = pluginData;
  env.PATH = pathWithoutNpx(); // deliberately no npx reachable anywhere
  env.LOCAL_RAG_TEST_GLOBAL_NODE_MODULES = noGlobal;
  const launcher = spawnLauncher(env);
  const getStderr = collectText(launcher.stderr);

  const lines = await expectReadyThenStop(launcher);
  assert.ok(!lines.some((l) => l.startsWith("NPX_ARGS")), `npx must never even be attempted: ${getStderr()}`);

  fs.rmSync(pluginData, { recursive: true, force: true });
  fs.rmSync(noGlobal, { recursive: true, force: true });
});
