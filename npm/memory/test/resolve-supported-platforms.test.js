"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");

const { resolvePlatformPackage, SUPPORTED_PLATFORMS } = require("../src/resolve.js");
const { buildFlatLayout } = require("./helpers/fixture-layout.js");
const { mkTmpRoot } = require("./helpers/tmp.js");

function tmpRoot() {
  return mkTmpRoot("lr-supported-");
}

test("every one of the five supported platforms resolves to its own distinct package", () => {
  const root = tmpRoot();
  const platformPackages = SUPPORTED_PLATFORMS.map((key) => ({
    name: `@13w/memory-${key}`,
    platform: key.split("-").slice(0, -1).join("-"),
    cpu: key.split("-").at(-1),
  }));
  const { launcherBinFile, packageDirs } = buildFlatLayout(root, platformPackages);

  const seenDirs = new Set();
  for (const key of SUPPORTED_PLATFORMS) {
    const [platform, arch] = [key.split("-").slice(0, -1).join("-"), key.split("-").at(-1)];
    const result = resolvePlatformPackage(launcherBinFile, { platform, arch });
    assert.equal(result.ok, true, `expected ${key} to resolve`);
    assert.equal(result.key, key);
    assert.equal(result.packageDir, packageDirs[`@13w/memory-${key}`]);
    assert.ok(!seenDirs.has(result.packageDir), `${key} must not reuse another platform's dir`);
    seenDirs.add(result.packageDir);
  }
  assert.equal(seenDirs.size, SUPPORTED_PLATFORMS.length);

  fs.rmSync(root, { recursive: true, force: true });
});

test("win32-arm64 is reported as deferred, not unsupported and not resolved", () => {
  const root = tmpRoot();
  const { launcherBinFile } = buildFlatLayout(root, []);
  const result = resolvePlatformPackage(launcherBinFile, { platform: "win32", arch: "arm64" });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "deferred");
  assert.equal(result.key, "win32-arm64");
  fs.rmSync(root, { recursive: true, force: true });
});

test("an exotic platform is reported as unsupported", () => {
  const root = tmpRoot();
  const { launcherBinFile } = buildFlatLayout(root, []);
  const result = resolvePlatformPackage(launcherBinFile, { platform: "freebsd", arch: "x64" });
  assert.equal(result.ok, false);
  assert.equal(result.reason, "unsupported");
  assert.equal(result.key, "freebsd-x64");
  assert.equal(result.packageName, null);
  fs.rmSync(root, { recursive: true, force: true });
});
