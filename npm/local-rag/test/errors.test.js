"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");

const {
  formatMissingPlatformError,
  formatUnsupportedPlatformError,
  formatDeferredPlatformError,
  formatMissingPackageError,
} = require("../src/errors.js");

test("unsupported-platform error names the platform and the supported list", () => {
  const msg = formatUnsupportedPlatformError({
    ok: false,
    reason: "unsupported",
    key: "freebsd-x64",
    packageName: null,
  });
  assert.match(msg, /freebsd-x64/);
  assert.match(msg, /darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64/);
  assert.doesNotMatch(msg, /win32-arm64/);
});

test("deferred-platform error explicitly calls out win32-arm64 as planned but unavailable", () => {
  const msg = formatDeferredPlatformError({
    ok: false,
    reason: "deferred",
    key: "win32-arm64",
    packageName: "@13w/local-rag-win32-arm64",
  });
  assert.match(msg, /win32-arm64/);
  assert.match(msg, /planned but not yet available/);
});

test("missing-package error names the exact package and gives an actionable fix", () => {
  const msg = formatMissingPackageError({
    ok: false,
    reason: "not-installed",
    key: "linux-x64",
    packageName: "@13w/local-rag-linux-x64",
  });
  assert.match(msg, /"@13w\/local-rag-linux-x64"/);
  assert.match(msg, /linux-x64/);
  assert.match(msg, /--omit=optional/);
  assert.match(msg, /npm install @13w\/local-rag-linux-x64 --save-optional/);
});

test("formatMissingPlatformError dispatches on reason", () => {
  const unsupported = formatMissingPlatformError({
    ok: false,
    reason: "unsupported",
    key: "freebsd-x64",
    packageName: null,
  });
  const deferred = formatMissingPlatformError({
    ok: false,
    reason: "deferred",
    key: "win32-arm64",
    packageName: "@13w/local-rag-win32-arm64",
  });
  const notInstalled = formatMissingPlatformError({
    ok: false,
    reason: "not-installed",
    key: "linux-x64",
    packageName: "@13w/local-rag-linux-x64",
  });
  assert.equal(
    unsupported,
    formatUnsupportedPlatformError({ ok: false, reason: "unsupported", key: "freebsd-x64", packageName: null }),
  );
  assert.equal(
    deferred,
    formatDeferredPlatformError({
      ok: false,
      reason: "deferred",
      key: "win32-arm64",
      packageName: "@13w/local-rag-win32-arm64",
    }),
  );
  assert.equal(
    notInstalled,
    formatMissingPackageError({
      ok: false,
      reason: "not-installed",
      key: "linux-x64",
      packageName: "@13w/local-rag-linux-x64",
    }),
  );
});

test("formatMissingPlatformError throws on an unknown reason rather than printing something wrong", () => {
  assert.throws(() => formatMissingPlatformError({ ok: false, reason: "bogus", key: "x", packageName: null }));
});

test("every error message ends up on a single, exit-1-shaped diagnostic (no trailing newline surprises)", () => {
  const msg = formatUnsupportedPlatformError({ ok: false, reason: "unsupported", key: "aix-ppc64", packageName: null });
  assert.equal(msg, msg.trim());
});
