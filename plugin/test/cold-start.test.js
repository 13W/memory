"use strict";

// Card requirement: "hook cold-start measurement (<50ms target)" — spec 13
// §1 `[FIXED]`. Measures the *steady-state* path only: one bootstrap run
// populates the `${CLAUDE_PLUGIN_DATA}` cache (legitimately slower — a
// one-time cost, not the budget this section targets), then N cached,
// direct-exec invocations of the real cargo-built binary are timed. The
// cached path is exactly what `plugin/hooks/hooks.json`'s fast-path branch
// execs — no Node/npx involved in the measured loop at all.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const { buildFlatLayout } = require("../../npm/memory/test/helpers/fixture-layout.js");
const { cachedHookPath } = require("../../npm/memory/src/hook-cache.js");
const { nativeHookBinaryPath } = require("./helpers/native-hook-binary.js");
const { prepareSpoolDir } = require("./helpers/store-fixture.js");

const nativeBin = nativeHookBinaryPath();
const SKIP_REASON = "target/debug/local-rag-hook is not built — run `cargo build -p local-rag-hook` first";

const MEASURED_ITERATIONS = 30;
const P95_BUDGET_MS = 50;

test(
  "steady-state (warm-cache) hook invocation stays under the 50ms cold-start budget",
  { skip: !nativeBin && SKIP_REASON },
  () => {
    const platformKey = `${process.platform}-${process.arch}`;
    const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-coldstart-")));
    const { launcherBinFile, packageDirs } = buildFlatLayout(root, [
      { name: `@13w/memory-${platformKey}`, platform: process.platform, cpu: process.arch },
    ]);
    const hookStubPath = path.join(packageDirs[`@13w/memory-${platformKey}`], "bin", "local-rag-hook");
    fs.rmSync(hookStubPath);
    fs.symlinkSync(nativeBin, hookStubPath);
    const hookJsFile = path.join(path.dirname(launcherBinFile), "local-rag-hook.js");

    const localRagHome = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-coldhome-"));
    const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-colddata-"));

    function eventJsonFor(sessionId) {
      return JSON.stringify({
        session_id: sessionId,
        hook_event_name: "SessionStart",
        cwd: root,
        source: "startup",
      });
    }

    // Bootstrap run — populates the cache; not part of the measured budget.
    prepareSpoolDir(localRagHome, "bootstrap");
    const bootstrap = spawnSync(process.execPath, [hookJsFile, "spool-write"], {
      input: eventJsonFor("bootstrap"),
      encoding: "utf8",
      env: { ...process.env, LOCAL_RAG_HOME: localRagHome, CLAUDE_PLUGIN_DATA: pluginData },
    });
    assert.equal(bootstrap.status, 0, `bootstrap run failed: ${bootstrap.stdout}\n${bootstrap.stderr}`);

    const cachedPath = cachedHookPath(pluginData);
    assert.ok(fs.existsSync(cachedPath), "bootstrap must populate the cache symlink");
    // The cache points at the *resolved package's* binary path (one hop),
    // which is itself a symlink to the real native binary in this fixture
    // — `refreshCache` never follows symlinks itself, it just records
    // `binaryPath()`'s own conventional path.
    assert.equal(fs.readlinkSync(cachedPath), hookStubPath);
    assert.equal(fs.realpathSync(cachedPath), fs.realpathSync(nativeBin));

    // Steady-state: exec the cached path directly, exactly like
    // `plugin/hooks/hooks.json`'s fast-path branch — no Node/npx here.
    const timingsMs = [];
    for (let i = 0; i < MEASURED_ITERATIONS; i++) {
      const sessionId = `steady-${i}`;
      prepareSpoolDir(localRagHome, sessionId);
      const start = process.hrtime.bigint();
      const r = spawnSync(cachedPath, ["spool-write"], {
        input: eventJsonFor(sessionId),
        encoding: "utf8",
        env: { ...process.env, LOCAL_RAG_HOME: localRagHome },
      });
      const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
      assert.equal(r.status, 0, `steady-state run ${i} failed: ${r.stdout}\n${r.stderr}`);
      timingsMs.push(elapsedMs);
    }

    timingsMs.sort((a, b) => a - b);
    const p50 = timingsMs[Math.floor(timingsMs.length * 0.5)];
    const p95 = timingsMs[Math.min(timingsMs.length - 1, Math.floor(timingsMs.length * 0.95))];
    console.log(
      `hook cold-start (steady-state, cached direct-exec, n=${timingsMs.length}): ` +
        `p50=${p50.toFixed(1)}ms p95=${p95.toFixed(1)}ms max=${timingsMs[timingsMs.length - 1].toFixed(1)}ms`,
    );
    assert.ok(p95 < P95_BUDGET_MS, `p95 ${p95.toFixed(1)}ms exceeds the ${P95_BUDGET_MS}ms budget`);

    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(localRagHome, { recursive: true, force: true });
    fs.rmSync(pluginData, { recursive: true, force: true });
  },
);
