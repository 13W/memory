"use strict";

// The launcher's own cold-start overhead against the `p95 < 100 ms` budget
// (`13 §1` `[SPEC]`).
//
// THE NUMBER IS NOT COMPARABLE WITH THE ONE IT REPLACES, and saying so is the
// point. The previous measurement (p50 ≈ 39 ms / p95 ≈ 42 ms, T19-03) timed the
// `${CLAUDE_PLUGIN_DATA}` cache tier — a `statSync` on a known path. That tier
// no longer exists: ADR-0013 Decision 3 removed it, and `13 §1`'s T22-04
// as-built note already records the measurement as superseded "because it timed
// that cache tier specifically and therefore no longer measures anything that
// exists". What is timed now is a real ordered scan of `PATH` and the
// well-known directories. A larger number here is a different thing being
// measured, not a regression.
//
// Both directions are timed, because the budget has to hold on the path a user
// hits when nothing is installed too — that is the case this whole group exists
// to make legible, and an unbounded failure path would be the worst of both.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");

const { TEST_DIRS_VAR, SERVER_BINARY, DAEMON_BINARY } = require("../bin/local-rag-mcp-launcher.js");
const { mkTmpRoot } = require("./helpers/tmp.js");
const { waitForStdoutLine } = require("./helpers/proc.js");
const { nativeBinaryPath } = require("./helpers/native-binary.js");

const LAUNCHER_FILE = path.join(__dirname, "..", "bin", "local-rag-mcp-launcher.js");
const FAKE_BINARY_SRC = fs.readFileSync(path.join(__dirname, "helpers", "fake-binary.js"), "utf8");

const MEASURED_ITERATIONS = 30;
const P95_BUDGET_MS = 100; // `13 §1` `[SPEC]`, chosen in T19-03 and unchanged.

function percentiles(timingsMs) {
  const sorted = [...timingsMs].sort((a, b) => a - b);
  return {
    p50: sorted[Math.floor(sorted.length * 0.5)],
    p95: sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * 0.95))],
    max: sorted[sorted.length - 1],
  };
}

function report(label, timingsMs) {
  const { p50, p95, max } = percentiles(timingsMs);
  console.log(
    `MCP launcher cold-start (${label}, n=${timingsMs.length}): ` +
      `p50=${p50.toFixed(1)}ms p95=${p95.toFixed(1)}ms max=${max.toFixed(1)}ms`,
  );
  return p95;
}

function installInto(dir) {
  fs.mkdirSync(dir, { recursive: true });
  const server = path.join(dir, SERVER_BINARY);
  fs.writeFileSync(server, FAKE_BINARY_SRC);
  fs.chmodSync(server, 0o755);
  const daemon = path.join(dir, DAEMON_BINARY);
  fs.writeFileSync(daemon, "#!/bin/sh\nexit 0\n");
  fs.chmodSync(daemon, 0o755);
  return dir;
}

test("a resolved server starts inside the p95 budget", async (t) => {
  const root = mkTmpRoot("lr-coldstart-ok-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  // A realistic list to walk: the resolved directory sits last, so the scan
  // pays for every rung before it rather than hitting on the first try.
  const decoys = ["/nonexistent-a", "/nonexistent-b", "/nonexistent-c"];
  const dir = installInto(path.join(root, "bin"));
  const env = {
    ...process.env,
    [TEST_DIRS_VAR]: [...decoys, dir].join(path.delimiter),
  };

  const timingsMs = [];
  for (let i = 0; i < MEASURED_ITERATIONS; i += 1) {
    const start = process.hrtime.bigint();
    const launcher = spawn(process.execPath, [LAUNCHER_FILE], {
      stdio: ["ignore", "pipe", "pipe"],
      env,
    });
    await waitForStdoutLine(launcher, (l) => l.startsWith("READY "));
    timingsMs.push(Number(process.hrtime.bigint() - start) / 1e6);
    launcher.kill("SIGTERM");
    await new Promise((resolve) => launcher.on("exit", resolve));
  }

  const p95 = report("resolved, 4 candidate dirs", timingsMs);
  assert.ok(p95 < P95_BUDGET_MS, `p95 ${p95.toFixed(1)}ms must stay under ${P95_BUDGET_MS}ms`);
});

test("the not-installed path is bounded by the same budget", async (t) => {
  // No `READY` line to wait for, so the clock runs to process exit instead.
  // Timing failure matters: a client that waits on a server which is never
  // coming should learn that fast.
  const root = mkTmpRoot("lr-coldstart-none-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const empty = path.join(root, "empty");
  fs.mkdirSync(empty, { recursive: true });
  const env = { ...process.env, [TEST_DIRS_VAR]: empty };

  const timingsMs = [];
  for (let i = 0; i < MEASURED_ITERATIONS; i += 1) {
    const start = process.hrtime.bigint();
    const launcher = spawn(process.execPath, [LAUNCHER_FILE], {
      stdio: ["ignore", "pipe", "pipe"],
      env,
    });
    const code = await new Promise((resolve) => launcher.on("exit", resolve));
    timingsMs.push(Number(process.hrtime.bigint() - start) / 1e6);
    assert.equal(code, 1);
  }

  const p95 = report("not installed", timingsMs);
  assert.ok(p95 < P95_BUDGET_MS, `p95 ${p95.toFixed(1)}ms must stay under ${P95_BUDGET_MS}ms`);
});

test("informational: handing off to the real native proxy, no daemon present", async (t) => {
  // Not gated on a budget — `local-rag-proxy` has no readiness signal, so the
  // only honest clock here runs to exit, which includes the proxy's own
  // fail-fast rather than the launcher's overhead alone.
  const proxy = nativeBinaryPath(SERVER_BINARY);
  const daemon = nativeBinaryPath(DAEMON_BINARY);
  if (proxy === null || daemon === null) {
    t.skip("cargo-built target/debug binaries are not present");
    return;
  }
  const root = mkTmpRoot("lr-coldstart-real-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const dir = path.join(root, "bin");
  fs.mkdirSync(dir, { recursive: true });
  fs.symlinkSync(proxy, path.join(dir, SERVER_BINARY));
  fs.symlinkSync(daemon, path.join(dir, DAEMON_BINARY));

  const env = { ...process.env, [TEST_DIRS_VAR]: dir };
  delete env.LOCAL_RAG_HOME;
  const start = process.hrtime.bigint();
  const launcher = spawn(process.execPath, [LAUNCHER_FILE], {
    stdio: ["ignore", "pipe", "pipe"],
    env,
  });
  await new Promise((resolve) => launcher.on("exit", resolve));
  const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
  const ms = elapsedMs.toFixed(1);
  console.log(`MCP launcher -> real local-rag-proxy handoff (no daemon): ${ms}ms`);
});
