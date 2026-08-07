"use strict";

// Card requirement: "paths per platform" — specific to the
// `npm/memory/src/binary-cache.js` layer this task adds (T17-01's own
// `resolve.js` per-platform resolution is already covered by its own
// suite; this does not duplicate that). Generalized (T19-03) from
// hook-only to cover both cached binaries (`local-rag-hook`,
// `local-rag-proxy`) that now share `${CLAUDE_PLUGIN_DATA}/bin/`.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const { refreshCache, cachedBinaryPath } = require("../../npm/memory/src/binary-cache.js");
const { SUPPORTED_PLATFORMS } = require("../../npm/memory/src/platform.js");

const CACHED_BINARY_NAMES = Object.freeze(["local-rag-hook", "local-rag-proxy"]);

for (const key of SUPPORTED_PLATFORMS) {
  const platform = key.split("-").slice(0, -1).join("-");
  for (const name of CACHED_BINARY_NAMES) {
    test(`${name} cache path for ${key} uses the correct suffix and points at the right binary`, () => {
      const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cache-path-"));
      const expectedSuffix = platform === "win32" ? ".exe" : "";
      const p = cachedBinaryPath(pluginData, name, platform);
      assert.ok(p.endsWith(name + expectedSuffix));
      assert.ok(!p.endsWith(name + (expectedSuffix === ".exe" ? "" : ".exe")));

      const fakeBinary = `/fake/resolved/binary/for/${key}/${name}`;
      refreshCache(fakeBinary, name, { pluginData, platform });
      assert.equal(fs.readlinkSync(p), fakeBinary);

      fs.rmSync(pluginData, { recursive: true, force: true });
    });
  }
}

test("cache paths for two different platforms never collide under the same pluginData root", () => {
  const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cache-collision-"));
  const posixPath = cachedBinaryPath(pluginData, "local-rag-hook", "linux");
  const winPath = cachedBinaryPath(pluginData, "local-rag-hook", "win32");
  assert.notEqual(posixPath, winPath);
  fs.rmSync(pluginData, { recursive: true, force: true });
});

test("cache paths for two different binary names never collide under the same pluginData root", () => {
  const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cache-name-collision-"));
  const hookPath = cachedBinaryPath(pluginData, "local-rag-hook", "linux");
  const proxyPath = cachedBinaryPath(pluginData, "local-rag-proxy", "linux");
  assert.notEqual(hookPath, proxyPath);
  fs.rmSync(pluginData, { recursive: true, force: true });
});

test("refreshCache is idempotent for the same (execPath, name, platform) triple — no error, no unnecessary write", () => {
  const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cache-idempotent-"));
  refreshCache("/some/binary", "local-rag-proxy", { pluginData, platform: "darwin" });
  const p = cachedBinaryPath(pluginData, "local-rag-proxy", "darwin");
  const mtimeBefore = fs.lstatSync(p).mtimeMs;
  refreshCache("/some/binary", "local-rag-proxy", { pluginData, platform: "darwin" });
  assert.equal(fs.lstatSync(p).mtimeMs, mtimeBefore, "an unchanged target must not rewrite the symlink");
  fs.rmSync(pluginData, { recursive: true, force: true });
});

test("refreshCache without CLAUDE_PLUGIN_DATA and no opts.pluginData is a silent no-op, not a throw", () => {
  const saved = process.env.CLAUDE_PLUGIN_DATA;
  delete process.env.CLAUDE_PLUGIN_DATA;
  try {
    assert.doesNotThrow(() => refreshCache("/some/binary", "local-rag-proxy", {}));
  } finally {
    if (saved !== undefined) {
      process.env.CLAUDE_PLUGIN_DATA = saved;
    }
  }
});
