"use strict";

// The resolution ladder (T22-09). Every test here builds the rungs it needs on
// disk and asserts which one won — the interesting property is never "a binary
// was found" but "this rung was preferred over that one", so each case
// populates at least two.
//
// `packageDir` is always a temp directory, never the real package. Anchored at
// the real one, the checkout rung wins unconditionally on a developer's
// machine and no test below would ever reach the rungs it is about.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const { PRODUCT_BINARIES, executableName } = require("../src/release.js");
const { targetTriple } = require("../src/platform.js");
const { MANIFEST_FILE, MANIFEST_VERSION } = require("../src/install.js");
const { locateBinDir, locateBinary, installInfo, BIN_DIR_VAR } = require("../src/locate.js");
const { mkTmpRoot } = require("./helpers/tmp.js");

const KEY = "linux-x64";
const PLATFORM = "linux";
const PKG_VERSION = "7.7.7";
const REQUIRED = PRODUCT_BINARIES.filter((b) => b.required).map((b) => b.name);
const ALL = PRODUCT_BINARIES.map((b) => b.name);

/** Executable stand-ins, the way an installer would have left them. */
function writeBinaries(dir, names, platform = PLATFORM) {
  fs.mkdirSync(dir, { recursive: true });
  for (const name of names) {
    const file = path.join(dir, executableName(name, platform));
    fs.writeFileSync(file, `#!/bin/sh\nexec echo ${name}\n`);
    fs.chmodSync(file, 0o755);
  }
  return dir;
}

function writeManifest(dir, names, overrides = {}) {
  const binaries = {};
  for (const name of ALL) {
    binaries[name] = names.includes(name)
      ? { state: "installed", file: executableName(name, PLATFORM) }
      : { state: "absent" };
  }
  const manifest = {
    manifestVersion: MANIFEST_VERSION,
    packageVersion: PKG_VERSION,
    platformKey: KEY,
    targetTriple: targetTriple(KEY),
    tag: "5.0.0",
    binaries,
    ...overrides,
  };
  fs.writeFileSync(path.join(dir, MANIFEST_FILE), `${JSON.stringify(manifest, null, 2)}\n`);
  return manifest;
}

/** A package root outside any checkout, plus a home for the cache rung. */
function scaffold(prefix) {
  const root = mkTmpRoot(`lr-locate-${prefix}-`);
  const packageDir = path.join(root, "pkg");
  const home = path.join(root, "home");
  fs.mkdirSync(packageDir, { recursive: true });
  fs.mkdirSync(home, { recursive: true });
  return {
    root,
    packageDir,
    packageBin: path.join(packageDir, "bin"),
    cacheBin: path.join(home, "local-rag", "bin", targetTriple(KEY)),
    opts: (extra = {}) => ({
      env: { LOCAL_RAG_HOME: home, ...(extra.env ?? {}) },
      key: KEY,
      platform: PLATFORM,
      packageDir,
      packageVersion: PKG_VERSION,
      ...extra,
    }),
  };
}

test("LOCAL_RAG_BIN_DIR wins over everything that would otherwise resolve", (t) => {
  const s = scaffold("override");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const override = writeBinaries(path.join(s.root, "vendored"), REQUIRED);
  writeBinaries(s.packageBin, REQUIRED);
  writeManifest(s.packageBin, REQUIRED);
  writeBinaries(s.cacheBin, REQUIRED);
  writeManifest(s.cacheBin, REQUIRED);

  const r = locateBinDir(s.opts({ env: { [BIN_DIR_VAR]: override } }));
  assert.equal(r.ok, true);
  assert.equal(r.source, "override");
  assert.equal(r.dir, override);
});

test("an override that does not hold the binaries is an error, never a fall-through", (t) => {
  // The rung below it would resolve perfectly well. Taking it would mean an
  // explicit instruction was read and discarded.
  const s = scaffold("override-missing");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const override = path.join(s.root, "empty");
  fs.mkdirSync(override, { recursive: true });
  writeBinaries(s.cacheBin, REQUIRED);
  writeManifest(s.cacheBin, REQUIRED);

  const r = locateBinDir(s.opts({ env: { [BIN_DIR_VAR]: override } }));
  assert.equal(r.ok, false);
  assert.equal(r.reason, "override-missing");
  assert.match(r.message, new RegExp(BIN_DIR_VAR));
  assert.match(r.message, /never silently ignored/);
  assert.match(r.message, new RegExp(override.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
});

test("the package's own bin beats the per-user cache", (t) => {
  const s = scaffold("pkg-beats-cache");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  writeBinaries(s.packageBin, REQUIRED);
  writeManifest(s.packageBin, REQUIRED);
  writeBinaries(s.cacheBin, REQUIRED);
  writeManifest(s.cacheBin, REQUIRED);

  const r = locateBinDir(s.opts());
  assert.equal(r.source, "package");
  assert.equal(r.dir, s.packageBin);
});

test("the cache is used when the package's own bin holds nothing", (t) => {
  const s = scaffold("cache");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  writeBinaries(s.cacheBin, REQUIRED);
  const manifest = writeManifest(s.cacheBin, REQUIRED);

  const r = locateBinDir(s.opts());
  assert.equal(r.source, "cache");
  assert.equal(r.dir, s.cacheBin);
  assert.equal(r.tag, manifest.tag);
});

test("a directory of binaries with no manifest is not trusted", (t) => {
  // Half an install looks exactly like this: the files are there and nothing
  // ever certified that they belong together.
  const s = scaffold("no-manifest");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  writeBinaries(s.packageBin, REQUIRED);
  writeBinaries(s.cacheBin, REQUIRED);
  writeManifest(s.cacheBin, REQUIRED);

  const r = locateBinDir(s.opts());
  assert.equal(r.source, "cache", "the unmanifested package bin was passed over");
  const passed = r.candidates.find((c) => c.source === "package");
  assert.match(passed.why, /manifest/);
});

test("a manifest written by a different wrapper version is not trusted", (t) => {
  const s = scaffold("foreign-version");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  writeBinaries(s.packageBin, REQUIRED);
  writeManifest(s.packageBin, REQUIRED, { packageVersion: "0.0.1" });
  writeBinaries(s.cacheBin, REQUIRED);
  writeManifest(s.cacheBin, REQUIRED);

  const r = locateBinDir(s.opts());
  assert.equal(r.source, "cache");
});

test("a directory holding the proxy but not the daemon is not a rung at all", (t) => {
  // Spec 13 §4: the daemon MUST sit beside the proxy that spawns it, because
  // "the version comes from whichever binary is found next to this proxy" is
  // what makes the upgrade trigger a definition. Handing back a directory
  // missing `local-rag` would move that failure to the moment the proxy looks
  // for its daemon and finds nothing.
  const s = scaffold("colocation");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  writeBinaries(s.packageBin, ["local-rag-proxy", "local-rag-hook"]);
  writeManifest(s.packageBin, ["local-rag-proxy", "local-rag-hook"]);
  writeBinaries(s.cacheBin, REQUIRED);
  writeManifest(s.cacheBin, REQUIRED);

  const r = locateBinDir(s.opts());
  assert.equal(r.source, "cache", "the incomplete directory was refused");
  const refused = r.candidates.find((c) => c.source === "package");
  assert.match(refused.why, /local-rag$/);
});

test("nothing anywhere is a typed failure that names the way out", (t) => {
  const s = scaffold("nothing");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));

  const r = locateBinDir(s.opts());
  assert.equal(r.ok, false);
  assert.equal(r.reason, "not-installed");
  assert.match(r.message, /npm install --global @13w\/memory/);
  assert.match(r.message, new RegExp(BIN_DIR_VAR));
  assert.deepEqual(
    r.candidates.map((c) => c.source),
    ["package", "cache"],
    "and it says which rungs it looked at",
  );
  assert.ok(r.candidates.every((c) => c.why !== null));
});

test("win32 looks for .exe, and says .exe when it is missing", (t) => {
  const s = scaffold("win32");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const override = path.join(s.root, "winbin");
  writeBinaries(override, REQUIRED, "win32");

  const ok = locateBinDir(
    s.opts({ platform: "win32", key: "win32-x64", env: { [BIN_DIR_VAR]: override } }),
  );
  assert.equal(ok.ok, true, "the .exe names were found");
  const one = locateBinary("local-rag-proxy", {
    ...s.opts({ platform: "win32", key: "win32-x64", env: { [BIN_DIR_VAR]: override } }),
  });
  assert.equal(path.basename(one.path), "local-rag-proxy.exe");

  const empty = path.join(s.root, "winempty");
  fs.mkdirSync(empty, { recursive: true });
  const bad = locateBinDir(
    s.opts({ platform: "win32", key: "win32-x64", env: { [BIN_DIR_VAR]: empty } }),
  );
  assert.match(bad.message, /local-rag\.exe/);
});

test("a platform with no release target is refused before any directory is read", (t) => {
  const s = scaffold("unsupported");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const r = locateBinDir(s.opts({ key: "win32-arm64", platform: "win32" }));
  assert.equal(r.ok, false);
  assert.equal(r.reason, "unsupported-platform");
  assert.deepEqual(r.candidates, []);
});

test("an optional binary the release did not carry is named as absent, not as uninstalled", (t) => {
  // Telling a user "nothing is installed" when three of four binaries are
  // sitting right there would send them to reinstall something that would not
  // change the answer.
  const s = scaffold("optional");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  writeBinaries(s.cacheBin, REQUIRED);
  writeManifest(s.cacheBin, REQUIRED);

  const r = locateBinary("local-rag-tui", s.opts());
  assert.equal(r.ok, false);
  assert.equal(r.reason, "binary-absent");
  assert.match(r.message, /local-rag-tui/);
  assert.match(r.message, /release 5\.0\.0/);
});

test("a non-executable file of the right name does not count as a binary", (t) => {
  const s = scaffold("nonexec");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const override = path.join(s.root, "notexec");
  writeBinaries(override, REQUIRED);
  fs.chmodSync(path.join(override, "local-rag"), 0o644);

  const r = locateBinDir(s.opts({ env: { [BIN_DIR_VAR]: override } }));
  assert.equal(r.ok, false, "reporting it found would only move the failure to exec time");
  assert.match(r.message, /local-rag is not an executable/);
});

test("installInfo reports the winning rung, the tag, and why the others lost", (t) => {
  const s = scaffold("info");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  writeBinaries(s.packageBin, REQUIRED);
  writeBinaries(s.cacheBin, REQUIRED);
  writeManifest(s.cacheBin, REQUIRED);

  const info = installInfo(s.opts());
  assert.equal(info.source, "cache");
  assert.equal(info.tag, "5.0.0");
  assert.equal(info.key, KEY);
  assert.equal(info.triple, targetTriple(KEY));
  assert.equal(info.packageVersion, PKG_VERSION);
  assert.equal(info.binaries["local-rag-proxy"].path, path.join(s.cacheBin, "local-rag-proxy"));
  assert.equal(info.binaries["local-rag-tui"].path, null);
  assert.equal(info.binaries["local-rag-tui"].required, false);
  assert.match(info.candidates.find((c) => c.source === "package").why, /manifest/);
});

test("installInfo surfaces the error a background repair left behind", (t) => {
  const s = scaffold("info-error");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  fs.mkdirSync(s.cacheBin, { recursive: true });
  fs.writeFileSync(path.join(s.cacheBin, ".local-rag-install.error"), "the sky fell\n");

  const info = installInfo(s.opts());
  assert.equal(info.source, null);
  assert.equal(info.reason, "not-installed");
  assert.equal(info.error, "the sky fell", "otherwise a detached repair fails invisibly");
});
