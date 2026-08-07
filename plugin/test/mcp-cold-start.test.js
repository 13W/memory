"use strict";

// Card requirement: "MCP-launcher — выбрать и задокументировать бюджет"
// (T19-03, spec 13 §1/§2). Unlike the hook's fast path (`hooks.json`
// execs the cached native binary directly via shell, no Node at all),
// `.mcp.json`'s `command` is a single statically-configured process with
// no shell chaining — Node startup is unavoidable on every tier. But the
// MCP server starts once per session, not once per event the way the hook
// does, so the relevant cost model is different: this measures the
// launcher's *own* resolve-to-exec overhead on the cached (tier 2)
// fast path, isolated from whatever the real native binary does after
// that — exactly mirroring `cold-start.test.js`'s own "steady-state,
// cached, direct path" framing for the hook.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const { nativeBinaryPath } = require("./helpers/native-binary.js");
const { waitForStdoutLine } = require("../../npm/memory/test/helpers/proc.js");

const LAUNCHER_FILE = path.join(__dirname, "..", "bin", "local-rag-mcp-launcher.js");
const FAKE_BINARY_SRC = fs.readFileSync(
  path.join(__dirname, "..", "..", "npm", "memory", "test", "helpers", "fake-binary.js"),
  "utf8",
);

const MEASURED_ITERATIONS = 30;
const P95_BUDGET_MS = 100; // T19-03, [SPEC] chosen — see docs/specification/13-distribution-and-migrations.md

function percentiles(timingsMs) {
  const sorted = [...timingsMs].sort((a, b) => a - b);
  return {
    p50: sorted[Math.floor(sorted.length * 0.5)],
    p95: sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * 0.95))],
    max: sorted[sorted.length - 1],
  };
}

test("launcher-only overhead on the cached (tier 2) fast path stays under the 100ms p95 budget", () => {
  const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-mcp-coldstart-"));
  const binDir = path.join(pluginData, "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const cacheFile = path.join(binDir, "local-rag-proxy" + (process.platform === "win32" ? ".exe" : ""));
  fs.writeFileSync(cacheFile, FAKE_BINARY_SRC);
  fs.chmodSync(cacheFile, 0o755);

  const env = { ...process.env, CLAUDE_PLUGIN_DATA: pluginData };
  delete env.CLAUDE_PROJECT_DIR; // force tier 2 (cache), isolating launcher-only overhead

  return (async () => {
    const timingsMs = [];
    for (let i = 0; i < MEASURED_ITERATIONS; i++) {
      const start = process.hrtime.bigint();
      const launcher = spawn(process.execPath, [LAUNCHER_FILE], {
        stdio: ["ignore", "pipe", "pipe"],
        env,
      });
      await waitForStdoutLine(launcher, (l) => l.startsWith("READY "));
      const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
      timingsMs.push(elapsedMs);

      launcher.kill("SIGTERM");
      await new Promise((resolve) => launcher.on("exit", resolve));
    }

    const { p50, p95, max } = percentiles(timingsMs);
    console.log(
      `MCP launcher cold-start (cached tier 2, n=${timingsMs.length}): ` +
        `p50=${p50.toFixed(1)}ms p95=${p95.toFixed(1)}ms max=${max.toFixed(1)}ms`,
    );
    assert.ok(p95 < P95_BUDGET_MS, `p95 ${p95.toFixed(1)}ms exceeds the ${P95_BUDGET_MS}ms budget`);

    fs.rmSync(pluginData, { recursive: true, force: true });
  })();
});

const nativeProxyBin = nativeBinaryPath("local-rag-proxy");
const SKIP_REASON =
  "target/debug/local-rag-proxy is not built — run `cargo build -p local-rag-proxy` first";

test(
  "informational only: launcher-to-real-native-binary handoff, no daemon present",
  { skip: !nativeProxyBin && SKIP_REASON },
  async () => {
    // local-rag-proxy has no defined "ready" signal to probe for a real
    // MCP session (it blocks relaying stdio once connected) — so unlike
    // the synthetic test above, this is not gated on a P95 budget. With no
    // daemon reachable in this test environment, the real binary fails
    // fast (`could not resolve the store directory` / no daemon socket)
    // and exits — the number logged here is launcher-resolve-to-exit
    // latency, evidence for PROGRESS.md, not an enforced property.
    const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-mcp-realcoldstart-"));
    const binDir = path.join(pluginData, "bin");
    fs.mkdirSync(binDir, { recursive: true });
    const cacheFile = path.join(binDir, "local-rag-proxy");
    fs.symlinkSync(nativeProxyBin, cacheFile);

    const env = { ...process.env, CLAUDE_PLUGIN_DATA: pluginData };
    delete env.CLAUDE_PROJECT_DIR;
    delete env.LOCAL_RAG_HOME;

    const start = process.hrtime.bigint();
    const launcher = spawn(process.execPath, [LAUNCHER_FILE], {
      stdio: ["ignore", "pipe", "pipe"],
      env,
    });
    await new Promise((resolve) => launcher.on("exit", resolve));
    const elapsedMs = Number(process.hrtime.bigint() - start) / 1e6;
    console.log(
      `MCP launcher -> real local-rag-proxy handoff (no daemon, fail-fast exit): ${elapsedMs.toFixed(1)}ms`,
    );

    fs.rmSync(pluginData, { recursive: true, force: true });
  },
);
