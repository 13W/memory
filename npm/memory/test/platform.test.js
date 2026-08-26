"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");

const fs = require("node:fs");
const path = require("node:path");

const {
  SUPPORTED_PLATFORMS,
  DEFERRED_PLATFORMS,
  TARGET_TRIPLES,
  platformKey,
  targetTriple,
  exeSuffix,
  platformPackageName,
  isSupported,
  isDeferred,
} = require("../src/platform.js");

// The producer's own list, read rather than retyped: a hand-copied array would
// only ever prove that this file agrees with itself.
function distWorkspaceTargets() {
  const toml = fs.readFileSync(
    path.join(__dirname, "..", "..", "..", "dist-workspace.toml"),
    "utf8",
  );
  const line = toml.split("\n").find((l) => l.trimStart().startsWith("targets"));
  assert.ok(line, "dist-workspace.toml must declare targets");
  return [...line.matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}

test("platformKey composes platform and arch", () => {
  assert.equal(platformKey("darwin", "arm64"), "darwin-arm64");
  assert.equal(platformKey("win32", "x64"), "win32-x64");
});

test("platformKey defaults to the real host when called with no arguments", () => {
  assert.equal(platformKey(), `${process.platform}-${process.arch}`);
});

test("platformPackageName is scoped under @13w", () => {
  assert.equal(platformPackageName("darwin-arm64"), "@13w/memory-darwin-arm64");
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

test("every supported platform maps to a target triple, and only those do", () => {
  for (const key of SUPPORTED_PLATFORMS) {
    assert.equal(typeof targetTriple(key), "string", `${key} must have a triple`);
  }
  assert.equal(targetTriple("win32-arm64"), null, "deferred key has no release asset");
  assert.equal(targetTriple("freebsd-x64"), null);
  assert.equal(Object.keys(TARGET_TRIPLES).length, SUPPORTED_PLATFORMS.length);
});

test("TARGET_TRIPLES is exactly dist-workspace.toml's target list", () => {
  assert.deepEqual(
    Object.values(TARGET_TRIPLES).sort(),
    distWorkspaceTargets().sort(),
  );
});

test("the key->triple map is not a positional zip of the two lists", () => {
  // The lists are ordered differently — dist-workspace.toml sorts by triple,
  // platform.js by key — so positions 2 and 3 disagree. This test fails loudly
  // if anyone ever "simplifies" the map into a zip.
  assert.equal(targetTriple("darwin-x64"), "x86_64-apple-darwin");
  assert.notEqual(targetTriple("darwin-x64"), distWorkspaceTargets()[1]);
});

test("exeSuffix is .exe only on win32", () => {
  assert.equal(exeSuffix("win32"), ".exe");
  for (const platform of ["darwin", "linux"]) {
    assert.equal(exeSuffix(platform), "");
  }
  assert.equal(exeSuffix(), process.platform === "win32" ? ".exe" : "");
});

test("TARGET_TRIPLES is frozen", () => {
  assert.ok(Object.isFrozen(TARGET_TRIPLES));
});
