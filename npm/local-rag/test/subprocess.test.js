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

const FAKE_BINARY_SRC = fs.readFileSync(path.join(__dirname, "helpers", "fake-binary.js"), "utf8");
const HOST_KEY = platformKey(); // real host's own platform-arch, e.g. "darwin-arm64"
const HOST_PACKAGE_NAME = `@13w/local-rag-${HOST_KEY}`;

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
  return { launcherBinFile, packageDir: packageDirs[HOST_PACKAGE_NAME] };
}

function spawnLauncher(launcherBinFile, args = []) {
  return spawn(process.execPath, [launcherBinFile, ...args], {
    stdio: ["ignore", "pipe", "pipe"],
  });
}

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
  const { launcherBinFile } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile);
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
  const { launcherBinFile } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile);
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
  const { launcherBinFile } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile);

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
  const launcher = spawnLauncher(launcherBinFile);
  const getStderr = collectText(launcher.stderr);

  const [code] = await new Promise((resolve) => launcher.on("exit", (c, s) => resolve([c, s])));
  assert.equal(code, 1);
  const stderr = getStderr();
  assert.match(stderr, new RegExp(HOST_KEY.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  assert.match(stderr, /local-rag:/);

  fs.rmSync(root, { recursive: true, force: true });
});

test("the child's own stdout/stderr reach the launcher's own stdout/stderr unchanged (stdio: inherit pass-through)", async () => {
  const root = mkTmpRoot("lr-sub-passthrough-");
  const { launcherBinFile } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile);

  const { lines } = await waitForStdoutLine(launcher, (l) => l.startsWith("READY "));
  assert.ok(lines.some((l) => l.startsWith("READY pid=")));

  process.kill(launcher.pid, "SIGTERM");
  await new Promise((resolve) => launcher.on("exit", resolve));

  fs.rmSync(root, { recursive: true, force: true });
});

test("accepted limitation: SIGKILL of the launcher itself cannot be forwarded, so the grandchild briefly survives it", async () => {
  const root = mkTmpRoot("lr-sub-sigkill-");
  const { launcherBinFile } = buildHostLayout(root);
  const launcher = spawnLauncher(launcherBinFile);

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
