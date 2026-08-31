"use strict";

// How the plugin finds the MCP server (T22-12), replacing the three-tier
// `mcp-launcher-tiers.test.js`. The subject changed completely: there is no npm
// package to resolve, no plugin-data cache to consult and no `npx` to fall back
// to — only a binary found by name in an ordered list of directories
// (spec 13 §2 `[FIXED, ADR-0013]`).
//
// Two styles, and the split is not cosmetic. `candidateBinDirs` and
// `resolveBinary` are called in-process, because they return rather than exit.
// Everything that spawns runs as a real subprocess: the success path ends in
// `process.exit()` inside `runChildAndExit`, so an in-process call would take
// the test runner with it.
//
// Nothing here imports from `npm/memory` — that is the card's acceptance
// criterion, and it is the point: the plugin has to work on a machine where
// that package was never installed, so a test suite that reaches into it is
// quietly asserting the opposite.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

const {
  BIN_DIR_VAR,
  TEST_DIRS_VAR,
  DEBUG_VAR,
  SERVER_BINARY,
  DAEMON_BINARY,
  candidateBinDirs,
  resolveBinary,
} = require("../bin/local-rag-mcp-launcher.js");
const { mkTmpRoot } = require("./helpers/tmp.js");
const { waitForStdoutLine, waitUntil, pidIsAlive } = require("./helpers/proc.js");

const LAUNCHER_FILE = path.join(__dirname, "..", "bin", "local-rag-mcp-launcher.js");
const FAKE_BINARY_SRC = fs.readFileSync(path.join(__dirname, "helpers", "fake-binary.js"), "utf8");

/** A `local-rag-proxy` stand-in that fails loudly if ever invoked — used to
 * prove a rung was never reached, not merely that the winner looks right. */
const POISON_BINARY_SRC =
  '#!/usr/bin/env node\nprocess.stderr.write("WRONG_DIR_INVOKED\\n");\nprocess.exit(1);\n';

/**
 * An installation: the server, plus the daemon beside it. The second file is
 * not decoration — spec 13 §4 requires the daemon to sit next to the proxy that
 * spawns it, and the resolver refuses a directory that has only one of them.
 */
function installInto(dir, { serverSrc = FAKE_BINARY_SRC, daemon = true } = {}) {
  fs.mkdirSync(dir, { recursive: true });
  const server = path.join(dir, SERVER_BINARY);
  fs.writeFileSync(server, serverSrc);
  fs.chmodSync(server, 0o755);
  if (daemon) {
    const d = path.join(dir, DAEMON_BINARY);
    fs.writeFileSync(d, "#!/bin/sh\nexit 0\n");
    fs.chmodSync(d, 0o755);
  }
  return dir;
}

function spawnLauncher(env) {
  return spawn(process.execPath, [LAUNCHER_FILE], { stdio: ["ignore", "pipe", "pipe"], env });
}

function collectText(stream) {
  const chunks = [];
  stream.on("data", (c) => chunks.push(c));
  return () => Buffer.concat(chunks).toString("utf8");
}

/**
 * The orphan-cleanup proof, carried over unchanged from the tiers file: after
 * the launcher exits on SIGTERM, the grandchild must be gone too. It is the
 * only assertion covering `runChildAndExit`'s `[FIXED list]` obligations.
 */
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

// ---------------------------------------------------------------------------
// candidateBinDirs — pure, no filesystem
// ---------------------------------------------------------------------------

test("the order is the override, then PATH in order, then the well-known directories", () => {
  const dirs = candidateBinDirs({
    platform: "linux",
    execPath: "/usr/lib/node/bin/node",
    env: { [BIN_DIR_VAR]: "/override", PATH: "/a:/b", HOME: "/home/u" },
  });
  assert.deepEqual(dirs.slice(0, 4), ["/override", "/a", "/b", "/usr/lib/node/bin"]);
  assert.ok(dirs.includes("/opt/homebrew/bin"));
  assert.ok(dirs.includes("/home/u/.local/share/pnpm"));
  // D-124: the commands live one level down on a real pnpm install, and naming
  // only the parent is what made a `pnpm link --global` unresolvable.
  assert.ok(dirs.includes("/home/u/.local/share/pnpm/bin"));
});

test("PNPM_HOME is derived, and it brings its bin child with it", () => {
  // The regression D-124 exists for. `pnpm setup` exports this variable, so it
  // is the machine's own answer and outranks the hard-coded default below it —
  // the same standing `dirname(execPath)` has, and for the same reason.
  const dirs = candidateBinDirs({
    platform: "linux",
    execPath: "/usr/lib/node/bin/node",
    env: { PATH: "/a", HOME: "/home/u", PNPM_HOME: "/home/u/.local/share/pnpm" },
  });
  assert.deepEqual(dirs.slice(0, 5), [
    "/a",
    "/usr/lib/node/bin",
    "/home/u/.local/share/pnpm",
    "/home/u/.local/share/pnpm/bin",
    "/opt/homebrew/bin",
  ]);
  // A pnpm home that is not the default is reached too — the point of reading
  // the variable rather than guessing the path.
  const moved = candidateBinDirs({
    platform: "linux",
    execPath: "/usr/lib/node/bin/node",
    env: { PATH: "", HOME: "/home/u", PNPM_HOME: "/opt/pnpm" },
  });
  assert.deepEqual(moved.slice(0, 3), ["/usr/lib/node/bin", "/opt/pnpm", "/opt/pnpm/bin"]);
});

test("a trailing separator on PNPM_HOME does not become a doubled one", () => {
  // Not cosmetic: `local-rag-resolve-hook.sh` must emit the byte-identical list
  // (T22-14), and there `"$d/bin"` on a value ending in `/` produces `//bin`.
  // The shell strips first; so does this, or the parity test fails.
  for (const raw of ["/opt/pnpm/", "/opt/pnpm//"]) {
    const dirs = candidateBinDirs({
      platform: "linux",
      execPath: "/usr/lib/node/bin/node",
      env: { PATH: "", HOME: "", PNPM_HOME: raw },
    });
    assert.deepEqual(dirs.slice(1, 3), ["/opt/pnpm", "/opt/pnpm/bin"], `PNPM_HOME=${raw}`);
  }
  // The degenerate value has one right answer rather than an empty string.
  const root = candidateBinDirs({
    platform: "linux",
    execPath: "/usr/lib/node/bin/node",
    env: { PATH: "", HOME: "", PNPM_HOME: "/" },
  });
  assert.deepEqual(root.slice(1, 3), ["/", "/bin"]);
});

test("the directory beside node is derived, not guessed", () => {
  // This is the entry that actually carries a global npm install, and it is
  // computed from `execPath` so it is right for nvm, fnm, volta, a system Node
  // and a Homebrew Node without naming any of them.
  const dirs = candidateBinDirs({
    platform: "linux",
    execPath: "/home/u/.nvm/versions/node/v24.0.0/bin/node",
    env: { PATH: "", HOME: "/home/u" },
  });
  assert.equal(dirs[0], "/home/u/.nvm/versions/node/v24.0.0/bin");
});

test("the test seam replaces the whole list rather than extending it", () => {
  // Extending would let the developer's real PATH decide a test's outcome, and
  // "it resolved" would stop meaning "it resolved where the test put it".
  const dirs = candidateBinDirs({
    env: {
      [TEST_DIRS_VAR]: `/a${path.delimiter}/b`,
      PATH: "/must/not/appear",
      [BIN_DIR_VAR]: "/nor/this",
    },
  });
  assert.deepEqual(dirs, ["/a", "/b"]);
});

test("win32 shapes are computed correctly from a POSIX host", () => {
  // D-055's trap: the ambient `path` module joins with the host separator, so a
  // cross-platform assertion would pass for the wrong reason.
  const dirs = candidateBinDirs({
    platform: "win32",
    execPath: "C:\\Program Files\\nodejs\\node.exe",
    env: {
      PATH: "C:\\Windows;C:\\bin",
      APPDATA: "C:\\Users\\u\\AppData\\Roaming",
      LOCALAPPDATA: "C:\\Users\\u\\AppData\\Local",
    },
  });
  assert.deepEqual(dirs, [
    "C:\\Windows",
    "C:\\bin",
    "C:\\Program Files\\nodejs",
    "C:\\Users\\u\\AppData\\Roaming\\npm",
    "C:\\Users\\u\\AppData\\Local\\pnpm",
    "C:\\Users\\u\\AppData\\Local\\pnpm\\bin",
  ]);
});

test("duplicates collapse to their first position, and an empty environment does not throw", () => {
  assert.deepEqual(
    candidateBinDirs({
      platform: "linux",
      execPath: "/a/node",
      env: { PATH: "/a:/b:/a", HOME: "" },
    }),
    ["/a", "/b", "/opt/homebrew/bin", "/usr/local/bin"],
  );
  assert.doesNotThrow(() => candidateBinDirs({ platform: "linux", execPath: "/a/node", env: {} }));
});

// ---------------------------------------------------------------------------
// resolveBinary — touches the filesystem, spawns nothing
// ---------------------------------------------------------------------------

test("the override wins over PATH, and the loser is never touched", (t) => {
  const root = mkTmpRoot("lr-resolve-override-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const winner = installInto(path.join(root, "override"));
  const loser = installInto(path.join(root, "onpath"), { serverSrc: POISON_BINARY_SRC });

  const found = resolveBinary(SERVER_BINARY, {
    platform: "linux",
    execPath: "/nowhere/node",
    env: { [BIN_DIR_VAR]: winner, PATH: loser, HOME: root },
  });
  assert.equal(found.dir, winner);
});

test("PATH wins over the well-known directories", (t) => {
  const root = mkTmpRoot("lr-resolve-path-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const onPath = installInto(path.join(root, "onpath"));
  const beside = installInto(path.join(root, "nodebin"), { serverSrc: POISON_BINARY_SRC });

  const found = resolveBinary(SERVER_BINARY, {
    platform: "linux",
    execPath: path.join(beside, "node"),
    env: { PATH: onPath, HOME: root },
  });
  assert.equal(found.dir, onPath);
});

test("a truncated PATH still finds a global install — the launchd case", (t) => {
  // A GUI-launched client inherits launchd's PATH, not the shell's. This rung
  // exists for exactly that, and ADR-0013 rejected a simpler design to keep it.
  const root = mkTmpRoot("lr-resolve-launchd-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const beside = installInto(path.join(root, "nodebin"));

  const found = resolveBinary(SERVER_BINARY, {
    platform: "linux",
    execPath: path.join(beside, "node"),
    env: { PATH: "/usr/bin:/bin:/usr/sbin:/sbin", HOME: root },
  });
  assert.equal(found.dir, beside);
});

test("a pnpm global install is found when it is the only rung left — D-124", (t) => {
  // The regression, in the shape it actually occurred. `pnpm link --global`
  // puts the commands in `$PNPM_HOME/bin`; the list named only `$PNPM_HOME`,
  // so with a truncated PATH and a Node the shims were never installed into,
  // nothing resolved and the client reported a server that had failed to
  // start. Both halves matter — hence the poisoned parent directory.
  const root = mkTmpRoot("lr-resolve-pnpm-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const pnpmHome = path.join(root, "pnpm");
  const realDir = installInto(path.join(pnpmHome, "bin"));
  fs.writeFileSync(path.join(pnpmHome, "not-a-binary"), "");

  const found = resolveBinary(SERVER_BINARY, {
    platform: "linux",
    // A Node with no global install beside it: the v24.20.0 half of the report.
    execPath: path.join(mkTmpRoot("lr-resolve-pnpm-node-"), "node"),
    env: { PATH: "/usr/bin:/bin", HOME: root, PNPM_HOME: pnpmHome },
  });
  assert.ok(found, "the install exists and must be reachable without PATH");
  assert.equal(found.dir, realDir);
});

test("the hard-coded pnpm default keeps working without PNPM_HOME", (t) => {
  // `pnpm setup` exports the variable, but a machine that installed pnpm some
  // other way has the directory and not the variable. Both rungs stay.
  const root = mkTmpRoot("lr-resolve-pnpm-default-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const dir = installInto(path.join(root, ".local", "share", "pnpm", "bin"));

  const found = resolveBinary(SERVER_BINARY, {
    platform: "linux",
    execPath: path.join(mkTmpRoot("lr-resolve-pnpm-default-node-"), "node"),
    env: { PATH: "/usr/bin:/bin", HOME: root },
  });
  assert.equal(found.dir, dir);
});

test("a directory holding the proxy but not the daemon is skipped", (t) => {
  // Spec 13 §4: the daemon MUST sit beside the proxy that spawns it, and
  // `connect.rs:55` is the code that depends on it. Accepting a half-directory
  // would move the failure to the moment the proxy looks for its daemon.
  const root = mkTmpRoot("lr-resolve-colocation-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const half = installInto(path.join(root, "half"), { daemon: false });
  const whole = installInto(path.join(root, "whole"));

  const found = resolveBinary(SERVER_BINARY, {
    platform: "linux",
    execPath: "/nowhere/node",
    env: { PATH: `${half}:${whole}`, HOME: root },
  });
  assert.equal(found.dir, whole);
});

test("a dangling symlink is not an executable", (t) => {
  const root = mkTmpRoot("lr-resolve-dangling-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const broken = path.join(root, "broken");
  fs.mkdirSync(broken, { recursive: true });
  fs.symlinkSync(path.join(root, "gone"), path.join(broken, SERVER_BINARY));
  fs.symlinkSync(path.join(root, "gone"), path.join(broken, DAEMON_BINARY));
  const whole = installInto(path.join(root, "whole"));

  const found = resolveBinary(SERVER_BINARY, {
    platform: "linux",
    execPath: "/nowhere/node",
    env: { PATH: `${broken}:${whole}`, HOME: root },
  });
  assert.equal(found.dir, whole);
});

test("nothing anywhere resolves to null rather than a guess", (t) => {
  const root = mkTmpRoot("lr-resolve-none-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  assert.equal(
    resolveBinary(SERVER_BINARY, {
      platform: "linux",
      execPath: path.join(root, "node"),
      env: { PATH: root, HOME: root },
    }),
    null,
  );
});

test("win32 looks for the .exe and not for the bare name", (t) => {
  const root = mkTmpRoot("lr-resolve-win32-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const dir = path.join(root, "bin");
  fs.mkdirSync(dir, { recursive: true });
  for (const name of [`${SERVER_BINARY}.exe`, `${DAEMON_BINARY}.exe`]) {
    fs.writeFileSync(path.join(dir, name), "#!/bin/sh\nexit 0\n");
    fs.chmodSync(path.join(dir, name), 0o755);
  }

  const found = resolveBinary(SERVER_BINARY, {
    platform: "win32",
    execPath: "C:\\node.exe",
    env: { [TEST_DIRS_VAR]: dir },
  });
  assert.ok(found.path.endsWith(`${SERVER_BINARY}.exe`));
  assert.ok(!found.path.endsWith(`${SERVER_BINARY}.exe.exe`));
});

// ---------------------------------------------------------------------------
// The real launcher, as a real process
// ---------------------------------------------------------------------------

const POSIX_ONLY = {
  skip:
    process.platform === "win32" &&
    "signal-forwarding tests are POSIX-only (see lifecycle.js's Windows note)",
};

test("a resolved server starts, and SIGTERM reaches the grandchild", POSIX_ONLY, async (t) => {
  const root = mkTmpRoot("lr-launch-ok-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const dir = installInto(path.join(root, "bin"));

  const launcher = spawnLauncher({ ...process.env, [TEST_DIRS_VAR]: dir });
  t.after(() => launcher.kill("SIGKILL"));
  await expectReadyThenStop(launcher);
});

test("nothing installed: stdout stays byte-empty, stderr names the way out, exit 1", async (t) => {
  // Spec 13 §2's per-channel contract. stdout is the JSON-RPC stream, so a
  // diagnostic there corrupts the framing before a client has read a byte; and
  // a non-zero exit is what makes the client show a failed server rather than a
  // silent one.
  const root = mkTmpRoot("lr-launch-none-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const empty = path.join(root, "empty");
  fs.mkdirSync(empty, { recursive: true });

  const launcher = spawnLauncher({ ...process.env, [TEST_DIRS_VAR]: empty });
  t.after(() => launcher.kill("SIGKILL"));
  const getStdout = collectText(launcher.stdout);
  const getStderr = collectText(launcher.stderr);
  const code = await new Promise((resolve) => launcher.on("exit", resolve));

  assert.equal(code, 1);
  assert.equal(getStdout(), "");
  const stderr = getStderr();
  assert.match(stderr, /npm i -g @13w\/memory/);
  assert.match(stderr, new RegExp(BIN_DIR_VAR));
  assert.doesNotMatch(stderr, /npx/, "the npx fallback is gone, and must not be advertised");
});

test("the candidate list is a debug affordance, not part of the error", async (t) => {
  const root = mkTmpRoot("lr-launch-debug-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const empty = path.join(root, "empty");
  fs.mkdirSync(empty, { recursive: true });
  const env = { ...process.env, [TEST_DIRS_VAR]: empty };

  const quiet = spawnLauncher(env);
  t.after(() => quiet.kill("SIGKILL"));
  const quietErr = collectText(quiet.stderr);
  await new Promise((resolve) => quiet.on("exit", resolve));
  assert.doesNotMatch(quietErr(), /looked in/);

  const loud = spawnLauncher({ ...env, [DEBUG_VAR]: "1" });
  t.after(() => loud.kill("SIGKILL"));
  const loudErr = collectText(loud.stderr);
  const loudOut = collectText(loud.stdout);
  await new Promise((resolve) => loud.on("exit", resolve));
  assert.match(loudErr(), /looked in/);
  assert.match(loudErr(), new RegExp(empty.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  assert.equal(loudOut(), "", "even the debug output stays off the protocol stream");
});

test("the launcher never runs npx, whatever the environment", async (t) => {
  // The tiers file asserted this negatively in every spawning test. Now it is
  // structural — there is no npx code path left — so one test that would catch
  // its return is enough.
  const root = mkTmpRoot("lr-launch-nonpx-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const marker = path.join(root, "npx-was-called");
  const fakeNpxDir = path.join(root, "fakebin");
  fs.mkdirSync(fakeNpxDir, { recursive: true });
  const npx = path.join(fakeNpxDir, "npx");
  fs.writeFileSync(npx, `#!/bin/sh\ntouch ${JSON.stringify(marker)}\nexit 0\n`);
  fs.chmodSync(npx, 0o755);

  const launcher = spawnLauncher({
    ...process.env,
    PATH: `${fakeNpxDir}${path.delimiter}${process.env.PATH}`,
    [TEST_DIRS_VAR]: path.join(root, "nothing-here"),
  });
  t.after(() => launcher.kill("SIGKILL"));
  await new Promise((resolve) => launcher.on("exit", resolve));
  assert.equal(fs.existsSync(marker), false, "npx must never be invoked");
});
