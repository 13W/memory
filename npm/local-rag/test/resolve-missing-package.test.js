"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");

const { resolvePlatformPackage } = require("../src/resolve.js");
const { buildFlatLayout } = require("./helpers/fixture-layout.js");
const { mkTmpRoot } = require("./helpers/tmp.js");

test("a supported platform whose package was never installed reports not-installed", () => {
  const root = mkTmpRoot("lr-missing-");
  // Only linux-x64 is present; darwin-arm64 is deliberately absent — the
  // realistic "npm install --omit=optional" / registry-hiccup case.
  const { launcherBinFile } = buildFlatLayout(root, [
    { name: "@13w/local-rag-linux-x64", platform: "linux", cpu: "x64" },
  ]);

  const result = resolvePlatformPackage(launcherBinFile, { platform: "darwin", arch: "arm64" });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "not-installed");
  assert.equal(result.key, "darwin-arm64");
  assert.equal(result.packageName, "@13w/local-rag-darwin-arm64");

  fs.rmSync(root, { recursive: true, force: true });
});

test("a missing package on a nonexistent node_modules tree entirely still reports not-installed, not a crash", () => {
  const root = mkTmpRoot("lr-missing-nonexistent-");
  // No node_modules at all under this launcher location.
  const fromFile = require("node:path").join(root, "bin", "local-rag-mcp.js");
  const result = resolvePlatformPackage(fromFile, { platform: "linux", arch: "x64" });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "not-installed");

  fs.rmSync(root, { recursive: true, force: true });
});

test("resolving one missing platform does not accidentally fall back to a different installed one", () => {
  const root = mkTmpRoot("lr-missing-no-fallback-");
  const { launcherBinFile } = buildFlatLayout(root, [
    { name: "@13w/local-rag-linux-x64", platform: "linux", cpu: "x64" },
    { name: "@13w/local-rag-darwin-x64", platform: "darwin", cpu: "x64" },
  ]);

  const result = resolvePlatformPackage(launcherBinFile, { platform: "darwin", arch: "arm64" });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "not-installed");
  assert.equal(result.packageName, "@13w/local-rag-darwin-arm64");

  fs.rmSync(root, { recursive: true, force: true });
});
