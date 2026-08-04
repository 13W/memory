"use strict";

// Card requirement: "paths per platform" — specific to the new
// `npm/local-rag/src/hook-cache.js` layer this task adds (T17-01's own
// `resolve.js` per-platform resolution is already covered by its own
// suite; this does not duplicate that).

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const { refreshCache, cachedHookPath } = require("../../npm/local-rag/src/hook-cache.js");
const { SUPPORTED_PLATFORMS } = require("../../npm/local-rag/src/platform.js");

for (const key of SUPPORTED_PLATFORMS) {
  const platform = key.split("-").slice(0, -1).join("-");
  test(`hook cache path for ${key} uses the correct suffix and points at the right binary`, () => {
    const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cache-path-"));
    const expectedSuffix = platform === "win32" ? ".exe" : "";
    const p = cachedHookPath(pluginData, platform);
    assert.ok(p.endsWith("local-rag-hook" + expectedSuffix));
    assert.ok(!p.endsWith("local-rag-hook" + (expectedSuffix === ".exe" ? "" : ".exe")));

    const fakeBinary = `/fake/resolved/binary/for/${key}`;
    refreshCache(fakeBinary, { pluginData, platform });
    assert.equal(fs.readlinkSync(p), fakeBinary);

    fs.rmSync(pluginData, { recursive: true, force: true });
  });
}

test("cache paths for two different platforms never collide under the same pluginData root", () => {
  const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cache-collision-"));
  const posixPath = cachedHookPath(pluginData, "linux");
  const winPath = cachedHookPath(pluginData, "win32");
  assert.notEqual(posixPath, winPath);
  fs.rmSync(pluginData, { recursive: true, force: true });
});

test("refreshCache is idempotent for the same (execPath, platform) pair — no error, no unnecessary write", () => {
  const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cache-idempotent-"));
  refreshCache("/some/binary", { pluginData, platform: "darwin" });
  const p = cachedHookPath(pluginData, "darwin");
  const mtimeBefore = fs.lstatSync(p).mtimeMs;
  refreshCache("/some/binary", { pluginData, platform: "darwin" });
  assert.equal(fs.lstatSync(p).mtimeMs, mtimeBefore, "an unchanged target must not rewrite the symlink");
  fs.rmSync(pluginData, { recursive: true, force: true });
});

test("refreshCache without CLAUDE_PLUGIN_DATA and no opts.pluginData is a silent no-op, not a throw", () => {
  const saved = process.env.CLAUDE_PLUGIN_DATA;
  delete process.env.CLAUDE_PLUGIN_DATA;
  try {
    assert.doesNotThrow(() => refreshCache("/some/binary", {}));
  } finally {
    if (saved !== undefined) {
      process.env.CLAUDE_PLUGIN_DATA = saved;
    }
  }
});
