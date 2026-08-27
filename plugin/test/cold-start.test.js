"use strict";

// Card requirement: "hook cold-start measurement (<50ms target)" — spec 13
// §1 `[FIXED]`.
//
// WHAT IS TIMED CHANGED IN T22-13, AND THE NEW NUMBER IS NOT COMPARABLE TO
// THE OLD ONE. This test used to time a bare `spawnSync` of a cached symlink,
// after a bootstrap run had populated `${CLAUDE_PLUGIN_DATA}` — that tier is
// gone (ADR-0013 Decision 3), and D-103 recorded that measuring the binary
// alone was never the same thing as measuring the hook path. What is timed
// now is the WHOLE `hooks.json` command line as Claude Code runs it: the fork
// of `/bin/sh`, the resolver's walk down the candidate list, the `exec`, and
// the native binary's own run. 13 §1 says the hooks path must be exec-fast,
// and the path is what this measures.
//
// `SessionStart` is timed rather than one of the six silent events because it
// is the heaviest of the seven — it carries the extra environment variable, it
// is the one that may print the notice, and the native binary attempts a
// recall RPC on it. If any line fits the budget, the other six do.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const { mkTmpRoot } = require("./helpers/tmp.js");
const { nativeBinaryPath, REPO_ROOT } = require("./helpers/native-binary.js");
const { prepareSpoolDir } = require("./helpers/store-fixture.js");
const { candidateBinDirs } = require("../bin/local-rag-mcp-launcher.js");

const PLUGIN_ROOT = path.join(REPO_ROOT, "plugin");
const nativeBin = nativeBinaryPath("local-rag-hook");
const SKIP_REASON = "target/debug/local-rag-hook is not built — run `cargo build -p local-rag-hook` first";

const MEASURED_ITERATIONS = 30;
const P95_BUDGET_MS = 50;

const SESSION_START_COMMAND = JSON.parse(
  fs.readFileSync(path.join(PLUGIN_ROOT, "hooks", "hooks.json"), "utf8"),
).hooks.SessionStart[0].hooks[0].command;

/**
 * Time `MEASURED_ITERATIONS` runs of the shipped command line against a given
 * candidate list, and assert the p95 against the budget.
 *
 * `LOCAL_RAG_TEST_BIN_DIRS` rather than `LOCAL_RAG_BIN_DIR` here, because what
 * varies between scenarios is the *length of the walk* — the seam that
 * replaces the whole list is the only way to state that exactly.
 */
function measure(label, binDirs, { expectStdout }) {
  const localRagHome = mkTmpRoot("lr-coldstart-home-");
  const fakeHome = mkTmpRoot("lr-coldstart-fakehome-");
  const timingsMs = [];

  for (let i = 0; i < MEASURED_ITERATIONS; i++) {
    const sessionId = `steady-${i}`;
    prepareSpoolDir(localRagHome, sessionId);
    const input = JSON.stringify({
      session_id: sessionId,
      hook_event_name: "SessionStart",
      cwd: localRagHome,
      source: "startup",
    });
    const start = process.hrtime.bigint();
    const r = spawnSync("/bin/sh", ["-c", SESSION_START_COMMAND], {
      input,
      encoding: "utf8",
      env: {
        PATH: "/usr/bin:/bin",
        HOME: fakeHome,
        CLAUDE_PLUGIN_ROOT: PLUGIN_ROOT,
        LOCAL_RAG_TEST_BIN_DIRS: binDirs.join(path.delimiter),
        LOCAL_RAG_HOME: localRagHome,
      },
    });
    const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
    // 11 §3.1 `[FIXED]` holds in the timed loop too, not only in the
    // functional test — a run that failed fast would make a fast p95.
    assert.equal(r.status, 0, `${label} run ${i} failed: ${r.stdout}\n${r.stderr}`);
    expectStdout(r.stdout, i);
    timingsMs.push(elapsedMs);
  }

  timingsMs.sort((a, b) => a - b);
  const p50 = timingsMs[Math.floor(timingsMs.length * 0.5)];
  const p95 = timingsMs[Math.min(timingsMs.length - 1, Math.floor(timingsMs.length * 0.95))];
  console.log(
    `hook path (${label}, whole hooks.json line, n=${timingsMs.length}): ` +
      `p50=${p50.toFixed(1)}ms p95=${p95.toFixed(1)}ms max=${timingsMs[timingsMs.length - 1].toFixed(1)}ms`,
  );
  assert.ok(p95 < P95_BUDGET_MS, `${label}: p95 ${p95.toFixed(1)}ms exceeds the ${P95_BUDGET_MS}ms budget`);

  fs.rmSync(localRagHome, { recursive: true, force: true });
  fs.rmSync(fakeHome, { recursive: true, force: true });
}

function binDirWithNativeHook() {
  const dir = mkTmpRoot("lr-coldstart-bin-");
  fs.symlinkSync(nativeBin, path.join(dir, "local-rag-hook"));
  return dir;
}

test(
  "the shipped hook command stays under the 50ms budget on a first-candidate hit",
  { skip: !nativeBin && SKIP_REASON },
  () => {
    measure("first candidate", [binDirWithNativeHook()], {
      expectStdout: (out) => assert.doesNotMatch(out, /not installed/),
    });
  },
);

test(
  "…and on a hit in the last of as many directories as this machine really has",
  { skip: !nativeBin && SKIP_REASON },
  () => {
    // The realistic worst case for a *successful* resolution: everything
    // before the binary is a miss. The list length is this machine's actual
    // candidate count rather than a made-up number, so the measurement stays
    // honest about how long the walk really is.
    const realLength = candidateBinDirs().length;
    assert.ok(realLength > 1, "a one-entry list would make this the same test as the one above");
    const empties = Array.from({ length: realLength - 1 }, () => mkTmpRoot("lr-coldstart-empty-"));
    measure(`last of ${realLength}`, [...empties, binDirWithNativeHook()], {
      expectStdout: (out) => assert.doesNotMatch(out, /not installed/),
    });
  },
);

test("…and on a complete miss, where the whole cost is the walk plus the notice", () => {
  // No native binary needed, so this one is never skipped: it is the branch a
  // user without an install actually takes, and 11 §3.1's exit 0 has to hold
  // there at the same price.
  const golden = fs.readFileSync(path.join(PLUGIN_ROOT, "hooks", "not-installed.json"), "utf8");
  const realLength = candidateBinDirs().length;
  const empties = Array.from({ length: realLength }, () => mkTmpRoot("lr-coldstart-empty-"));
  measure(`miss over ${realLength}`, empties, {
    expectStdout: (out) => assert.equal(out, golden),
  });
});
