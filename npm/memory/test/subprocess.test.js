"use strict";

// Real-OS-process tier, mirroring `crates/local-rag-proxy/tests/subprocess.rs`
// in both name and philosophy: genuine child processes, real `process.kill`
// with an exact signal, deadline-polling instead of a fixed sleep, no
// mocking framework. POSIX-gated like that file's own `#![cfg(unix)]` —
// `child.kill('SIGTERM')` on win32 is an unconditional `TerminateProcess`
// with no graceful-delivery semantics to prove here (see `../src/
// lifecycle.js`'s own doc comment).

if (process.platform === "win32") {
  const { test } = require("node:test");
  test("subprocess signal-forwarding tests are POSIX-only (see lifecycle.js's Windows note)", { skip: true }, () => {});
  return;
}

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

const { platformKey } = require("../src/platform.js");
const { buildFlatLayout } = require("./helpers/fixture-layout.js");
const { mkTmpRoot } = require("./helpers/tmp.js");
const { waitUntil, pidIsAlive, waitForStdoutLine } = require("./helpers/proc.js");
const { startFixtureRelease } = require("./helpers/fixture-server.js");

const FAKE_BINARY_SRC = fs.readFileSync(path.join(__dirname, "helpers", "fake-binary.js"), "utf8");
const HOST_KEY = platformKey(); // real host's own platform-arch, e.g. "darwin-arm64"
const HOST_PACKAGE_NAME = `@13w/memory-${HOST_KEY}`;

/** Builds a fixture whose `local-rag-proxy` binary is the real fake-binary.js script. */
function buildHostLayout(root, opts = {}) {
  const { launcherBinFile, packageDirs } = buildFlatLayout(root, [
    {
      name: HOST_PACKAGE_NAME,
      platform: process.platform,
      cpu: process.arch,
      binaryContents: { "local-rag-proxy": FAKE_BINARY_SRC },
      ...opts,
    },
  ]);
  // The shim T22-10 ships, not the one it replaced. This file is the only
  // real-process coverage of `lifecycle.js`'s signal forwarding and of the
  // not-installed exit contract, so it has to test the entry point that
  // actually gets installed — `bin/local-rag-mcp.js` still exists beside it,
  // but only until T22-12 stops resolving it.
  const stub = path.join(path.dirname(launcherBinFile), "local-rag-proxy");
  const packageDir = packageDirs[HOST_PACKAGE_NAME];
  return { launcherBinFile: stub, packageDir, binDir: path.join(packageDir, "bin") };
}

/**
 * `LOCAL_RAG_HOME` is always set, even for the failure cases: without it the
 * resolver's last rung is the developer's real per-user cache, and a test that
 * consults it is neither hermetic nor honest. `LOCAL_RAG_BIN_DIR` stands in for
 * the install — `writePlatformPackageAt` writes exactly the required set, which
 * is what the resolver demands of a directory before it will use one.
 *
 * `LOCAL_RAG_RELEASE_BASE_URL` is pinned to a closed loopback port by default,
 * and that is not belt-and-braces. The MCP stub heals synchronously before it
 * reports a failure, so a not-installed run really does try to fetch — measured
 * reaching github.com in 576 ms before this line existed. A test that wants the
 * heal to get somewhere points this at its own fixture server instead.
 */
function spawnLauncher(launcherBinFile, args = [], env = {}) {
  return spawn(process.execPath, [launcherBinFile, ...args], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      LOCAL_RAG_HOME: EMPTY_HOME,
      LOCAL_RAG_RELEASE_BASE_URL: "http://127.0.0.1:1/releases",
      ...env,
    },
  });
}

/** A guaranteed-empty store root, so the cache rung can never resolve. */
const EMPTY_HOME = mkTmpRoot("lr-subprocess-home-");

function collectText(stream) {
  const chunks = [];
  stream.on("data", (c) => chunks.push(c));
  return () => Buffer.concat(chunks).toString("utf8");
}

async function readReadyPid(launcher) {
  const { line } = await waitForStdoutLine(launcher, (l) => l.startsWith("READY "));
  const match = /^READY pid=(\d+)/.exec(line);
  assert.ok(match, `expected a READY line, got: ${line}`);
  return Number(match[1]);
}

test("SIGTERM sent to the launcher is forwarded to the child and the launcher exits 0", async () => {
  const root = mkTmpRoot("lr-sub-sigterm-");
  const { launcherBinFile, binDir } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile, [], { LOCAL_RAG_BIN_DIR: binDir });
  const getStderr = collectText(launcher.stderr);

  const childPid = await readReadyPid(launcher);
  assert.ok(pidIsAlive(childPid));

  process.kill(launcher.pid, "SIGTERM");

  const [code] = await new Promise((resolve) => launcher.on("exit", (c, s) => resolve([c, s])));
  assert.equal(code, 0, `launcher stderr: ${getStderr()}`);

  await waitUntil(() => !pidIsAlive(childPid), { description: "grandchild to exit after SIGTERM" });

  fs.rmSync(root, { recursive: true, force: true });
});

test("SIGINT sent to the launcher is forwarded to the child and the launcher exits 0", async () => {
  const root = mkTmpRoot("lr-sub-sigint-");
  const { launcherBinFile, binDir } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile, [], { LOCAL_RAG_BIN_DIR: binDir });
  const getStderr = collectText(launcher.stderr);

  const childPid = await readReadyPid(launcher);

  process.kill(launcher.pid, "SIGINT");

  const [code] = await new Promise((resolve) => launcher.on("exit", (c, s) => resolve([c, s])));
  assert.equal(code, 0, `launcher stderr: ${getStderr()}`);
  await waitUntil(() => !pidIsAlive(childPid), { description: "grandchild to exit after SIGINT" });

  fs.rmSync(root, { recursive: true, force: true });
});

test("no orphan: once the launcher has exited after SIGTERM, the grandchild process is gone too", async () => {
  const root = mkTmpRoot("lr-sub-noorphan-");
  const { launcherBinFile, binDir } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile, [], { LOCAL_RAG_BIN_DIR: binDir });

  const childPid = await readReadyPid(launcher);
  process.kill(launcher.pid, "SIGTERM");
  await new Promise((resolve) => launcher.on("exit", resolve));

  // The launcher process itself is confirmed exited (the event already
  // fired); the orphan check is specifically about the *grandchild*.
  assert.equal(pidIsAlive(childPid), false, "the fake-binary grandchild must not outlive the launcher");

  fs.rmSync(root, { recursive: true, force: true });
});

test("missing platform package end to end: the real launcher prints an actionable diagnostic and exits 1", async () => {
  const root = mkTmpRoot("lr-sub-missing-");
  // Deliberately do NOT install the real host's own platform package —
  // computed dynamically so this test passes on every one of the 5 CI
  // hosts without hardcoding one platform.
  const { launcherBinFile } = buildFlatLayout(root, []);
  const stub = path.join(path.dirname(launcherBinFile), "local-rag-proxy");
  // A release that resolves a tag and then carries nothing. The MCP stub heals
  // before it reports, so it has to be given somewhere to try — and a real host
  // would make this test depend on the network and on a published release's
  // state. The fixture answers in milliseconds and records that the attempt
  // happened at all.
  const server = await startFixtureRelease({ tag: "0.0.0", assets: {} });
  const launcher = spawnLauncher(stub, [], { LOCAL_RAG_RELEASE_BASE_URL: server.origin });
  const getStdout = collectText(launcher.stdout);
  const getStderr = collectText(launcher.stderr);

  const [code] = await new Promise((resolve) => launcher.on("exit", (c, s) => resolve([c, s])));
  assert.equal(code, 1);
  const stderr = getStderr();
  assert.match(stderr, new RegExp(HOST_KEY.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  assert.match(stderr, /local-rag:/);
  // stdout is the JSON-RPC stream on this path; a diagnostic there corrupts the
  // framing before a client has read a byte.
  assert.equal(getStdout(), "");
  assert.ok(server.requestCount() > 0, "the stub tried to heal before giving up");

  await server.close();
  fs.rmSync(root, { recursive: true, force: true });
});

test("the child's own stdout/stderr reach the launcher's own stdout/stderr unchanged (stdio: inherit pass-through)", async () => {
  const root = mkTmpRoot("lr-sub-passthrough-");
  const { launcherBinFile, binDir } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile, [], { LOCAL_RAG_BIN_DIR: binDir });

  const { lines } = await waitForStdoutLine(launcher, (l) => l.startsWith("READY "));
  assert.ok(lines.some((l) => l.startsWith("READY pid=")));

  process.kill(launcher.pid, "SIGTERM");
  await new Promise((resolve) => launcher.on("exit", resolve));

  fs.rmSync(root, { recursive: true, force: true });
});

test("accepted limitation: SIGKILL of the launcher itself cannot be forwarded, so the grandchild briefly survives it", async () => {
  const root = mkTmpRoot("lr-sub-sigkill-");
  const { launcherBinFile, binDir } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile, [], { LOCAL_RAG_BIN_DIR: binDir });

  const childPid = await readReadyPid(launcher);

  // SIGKILL is uncatchable by definition — the launcher cannot run any of
  // its own forwarding logic in response to it. This is a deliberate,
  // permanent limitation (see `../src/lifecycle.js`'s doc comment), and
  // this test characterizes it as an executed assertion rather than
  // leaving it as unverified prose.
  process.kill(launcher.pid, "SIGKILL");
  await new Promise((resolve) => launcher.on("exit", resolve));

  assert.ok(pidIsAlive(childPid), "the grandchild is expected to still be alive right after an uncatchable SIGKILL");

  // Clean up the now-orphaned grandchild ourselves so the test suite does
  // not leak a live process.
  process.kill(childPid, "SIGKILL");
  await waitUntil(() => !pidIsAlive(childPid), { description: "manual cleanup of the orphaned grandchild" });

  fs.rmSync(root, { recursive: true, force: true });
});
