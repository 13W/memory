"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");

const {
  SUPPORTED_PLATFORMS,
  DEFERRED_PLATFORMS,
  platformKey,
  platformPackageName,
  isSupported,
  isDeferred,
} = require("../src/platform.js");

test("platformKey composes platform and arch", () => {
  assert.equal(platformKey("darwin", "arm64"), "darwin-arm64");
  assert.equal(platformKey("win32", "x64"), "win32-x64");
});

test("platformKey defaults to the real host when called with no arguments", () => {
  assert.equal(platformKey(), `${process.platform}-${process.arch}`);
});

test("platformPackageName is scoped under @13w", () => {
  assert.equal(platformPackageName("darwin-arm64"), "@13w/local-rag-darwin-arm64");
});

test("exactly the five v0 platform targets are supported (spec 13 §1/15 §2)", () => {
  assert.deepEqual(
    [...SUPPORTED_PLATFORMS].sort(),
    ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64"].sort(),
  );
});

test("win32-arm64 is deferred, not supported", () => {
  assert.equal(isSupported("win32-arm64"), false);
  assert.equal(isDeferred("win32-arm64"), true);
  assert.ok(DEFERRED_PLATFORMS.includes("win32-arm64"));
});

test("an exotic platform is neither supported nor deferred", () => {
  assert.equal(isSupported("freebsd-x64"), false);
  assert.equal(isDeferred("freebsd-x64"), false);
});

test("SUPPORTED_PLATFORMS and DEFERRED_PLATFORMS are frozen", () => {
  assert.ok(Object.isFrozen(SUPPORTED_PLATFORMS));
  assert.ok(Object.isFrozen(DEFERRED_PLATFORMS));
});
