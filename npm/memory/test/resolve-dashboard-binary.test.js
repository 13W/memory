"use strict";

// T18-01: the `local-rag-dashboard` launcher (`bin/local-rag-dashboard.js`) resolves the native
// `local-rag-tui` binary the same way `local-rag-mcp.js` resolves `local-rag-proxy` — modeled on
// T17-01's own `resolve-layout-matrix.test.js`. Re-proving layout-generality
// (flat/nested/pnpm-symlinked) here would be redundant: that property belongs to
// `resolvePlatformPackage`'s own directory walk, already exhaustively covered for one binary name
// by T17-01, and does not vary by which binary name `binaryPath` is asked to join — so this file
// exercises exactly one layout (flat) plus the pure `binaryPath` computation for the new name.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const { resolvePlatformPackage, binaryPath } = require("../src/resolve.js");
const { buildFlatLayout, DEFAULT_BINARIES } = require("./helpers/fixture-layout.js");
const { mkTmpRoot } = require("./helpers/tmp.js");

const PLATFORM_PACKAGE = {
  name: "@13w/memory-linux-x64",
  platform: "linux",
  cpu: "x64",
  binaries: [...DEFAULT_BINARIES, "local-rag-tui"],
};

test("resolves the local-rag-tui binary path for the current platform", () => {
  const root = mkTmpRoot("lr-dashboard-");
  const { launcherBinFile, packageDirs } = buildFlatLayout(root, [PLATFORM_PACKAGE]);

  const result = resolvePlatformPackage(launcherBinFile, { platform: "linux", arch: "x64" });
  assert.equal(result.ok, true);
  assert.equal(result.packageDir, packageDirs["@13w/memory-linux-x64"]);

  const execPath = binaryPath(result.packageDir, "linux", "local-rag-tui");
  assert.equal(execPath, path.join(result.packageDir, "bin", "local-rag-tui"));
  assert.ok(fs.existsSync(execPath), "the fixture must place local-rag-tui in the resolved package dir");

  fs.rmSync(root, { recursive: true, force: true });
});

test("binaryPath appends the .exe suffix for local-rag-tui on win32", () => {
  const execPath = binaryPath("/pkg", "win32", "local-rag-tui");
  assert.equal(execPath, path.join("/pkg", "bin", "local-rag-tui.exe"));
});
