"use strict";

// Card requirement: "mocked layout matrix" — resolution must work
// identically regardless of which package manager's `node_modules` shape
// actually installed the packages (spec 13 §2: "resolution under
// pnpm/npm/yarn layouts (hoisting differences)").

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");

const { resolvePlatformPackage } = require("../src/resolve.js");
const { buildFlatLayout, buildNestedLayout, buildPnpmLayout } = require("./helpers/fixture-layout.js");
const { mkTmpRoot } = require("./helpers/tmp.js");

const PLATFORM_PACKAGE = { name: "@13w/memory-linux-x64", platform: "linux", cpu: "x64" };

const LAYOUTS = [
  {
    label: "flat/hoisted (plain npm, yarn classic)",
    build: buildFlatLayout,
  },
  {
    label: "nested/unhoisted (platform package inside the launcher's own node_modules)",
    build: buildNestedLayout,
  },
  {
    label: "pnpm-style (content-addressed store + real symlink chain)",
    build: buildPnpmLayout,
  },
];

for (const { label, build } of LAYOUTS) {
  test(`resolves the correct platform package under a ${label} layout`, () => {
    const root = mkTmpRoot("lr-layout-");
    const { launcherBinFile, packageDirs } = build(root, [PLATFORM_PACKAGE]);

    assert.ok(fs.existsSync(launcherBinFile), "the fixture must actually place the launcher bin file");

    const result = resolvePlatformPackage(launcherBinFile, { platform: "linux", arch: "x64" });
    assert.equal(result.ok, true, `expected linux-x64 to resolve under the ${label} layout`);
    assert.equal(result.key, "linux-x64");
    assert.equal(result.packageName, "@13w/memory-linux-x64");
    assert.equal(result.packageDir, packageDirs["@13w/memory-linux-x64"]);
    assert.ok(
      fs.existsSync(require("node:path").join(result.packageDir, "bin", "local-rag-proxy")),
      "the resolved package dir must contain the product binaries",
    );

    fs.rmSync(root, { recursive: true, force: true });
  });
}

test("the pnpm layout's platform package genuinely lives behind a real symlink chain (not accidentally flattened by the fixture builder)", () => {
  const root = mkTmpRoot("lr-layout-pnpm-symlink-proof-");
  const { launcherBinFile, packageDirs } = buildPnpmLayout(root, [PLATFORM_PACKAGE]);

  // The launcher's own on-disk location is itself reached only via a
  // symlink from the project-level node_modules into the .pnpm store.
  const launcherDir = require("node:path").dirname(require("node:path").dirname(launcherBinFile));
  const stat = fs.lstatSync(launcherDir);
  assert.ok(stat.isSymbolicLink(), "the launcher package dir must be a symlink under a pnpm-style layout");

  // The platform package is reached through a *second*, independent
  // symlink (the launcher's own private node_modules/@13w/<platform>).
  const launcherPrivateLink = require("node:path").join(
    launcherDir,
    "node_modules",
    "@13w",
    "memory-linux-x64",
  );
  assert.ok(fs.lstatSync(launcherPrivateLink).isSymbolicLink());

  const result = resolvePlatformPackage(launcherBinFile, { platform: "linux", arch: "x64" });
  assert.equal(result.ok, true);
  assert.equal(result.packageDir, packageDirs["@13w/memory-linux-x64"]);

  fs.rmSync(root, { recursive: true, force: true });
});

test("a package present under one layout kind but absent under another still reports not-installed correctly per layout", () => {
  const root = mkTmpRoot("lr-layout-partial-");
  // Nested layout, but the requested platform package was never written.
  const { launcherBinFile } = buildNestedLayout(root, []);
  const result = resolvePlatformPackage(launcherBinFile, { platform: "linux", arch: "x64" });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "not-installed");
  fs.rmSync(root, { recursive: true, force: true });
});
