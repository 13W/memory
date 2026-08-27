"use strict";

// Swapping the JS stubs for the native binaries (T22-10). POSIX only: the whole
// mechanism rests on npm linking `.bin` entries as symlinks, which it does
// unconditionally off Windows and never on it.
//
// The interesting assertion is not "the file changed" but *how*. A hard link is
// taken to each stub before the install and checked afterwards: that link is
// what a shared package store holds, and if the replacement wrote through the
// path instead of replacing the directory entry, the link would show the new
// bytes. It is reproducible in three lines, so it is worth a test rather than a
// comment.

if (process.platform === "win32") {
  const { test } = require("node:test");
  const why = "bin replacement is POSIX-only (npm writes .cmd/.ps1 wrappers on Windows)";
  test(why, { skip: true }, () => {});
  return;
}

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");
const { spawn, spawnSync, execFileSync } = require("node:child_process");

const { PRODUCT_BINARIES, assetName, sidecarName, executableName } = require("../src/release.js");
const { platformKey, targetTriple } = require("../src/platform.js");
const { DISABLE_VAR } = require("../src/replace-shims.js");
const { writeLauncherPackageAt } = require("./helpers/fixture-layout.js");
const { startFixtureRelease } = require("./helpers/fixture-server.js");
const { mkTmpRoot } = require("./helpers/tmp.js");
const { buildTar, buildZip } = require("./helpers/archive-fixtures.js");

const KEY = platformKey();
const PLATFORM = process.platform;
const NPM_AGENT = "npm/11.16.0 node/v24.18.1 darwin arm64 workspaces/false";

/** A whole release whose binaries announce themselves. */
function releaseAssets() {
  const assets = {};
  const bodies = {};
  for (const binary of PRODUCT_BINARIES) {
    const asset = assetName(binary.name, KEY);
    const build = asset.endsWith(".zip") ? buildZip : buildTar;
    const body = Buffer.from(`#!/bin/sh\necho "NATIVE ${binary.name}"\n`);
    const archive = build([{ name: executableName(binary.name, PLATFORM), data: body }]);
    assets[asset] = archive;
    assets[sidecarName(asset)] =
      `${crypto.createHash("sha256").update(archive).digest("hex")} *${asset}\n`;
    bodies[binary.name] = body;
  }
  return { assets, bodies };
}

function scaffold(prefix) {
  const root = mkTmpRoot(`lr-replace-${prefix}-`);
  const pkg = path.join(root, "node_modules", "@13w", "memory");
  writeLauncherPackageAt(pkg);
  return {
    root,
    pkg,
    home: path.join(root, "home"),
    binOf: (name) => path.join(pkg, "bin", executableName(name, PLATFORM)),
  };
}

/**
 * Asynchronous on purpose, and it is not a style choice. The fixture server
 * runs inside this very process, so a `spawnSync` here would block the event
 * loop that has to answer the child's requests — the child waits for a reply
 * that cannot come until it exits. That deadlock is silent: the run simply
 * never finishes.
 */
function runPostinstall(s, env = {}) {
  const child = spawn(process.execPath, [path.join(s.pkg, "scripts", "postinstall.js")], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      LOCAL_RAG_HOME: s.home,
      npm_config_user_agent: NPM_AGENT,
      ...env,
    },
  });
  const out = [];
  const err = [];
  child.stdout.on("data", (c) => out.push(c));
  child.stderr.on("data", (c) => err.push(c));
  return new Promise((resolve) => {
    child.on("exit", (status) =>
      resolve({
        status,
        stdout: Buffer.concat(out).toString("utf8"),
        stderr: Buffer.concat(err).toString("utf8"),
      }),
    );
  });
}

test("after postinstall the command is the native file itself, byte for byte", async (t) => {
  const { assets, bodies } = releaseAssets();
  const server = await startFixtureRelease({ tag: "3.3.0", assets });
  t.after(() => server.close());
  const s = scaffold("happy");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));

  const result = await runPostinstall(s, { LOCAL_RAG_RELEASE_BASE_URL: server.origin });
  assert.equal(result.status, 0, result.stderr);

  for (const binary of PRODUCT_BINARIES) {
    const file = s.binOf(binary.name);
    assert.deepEqual(fs.readFileSync(file), bodies[binary.name], binary.name);
    assert.ok(fs.statSync(file).mode & 0o111, `${binary.name} must stay executable`);
  }
  // One file on disk, two names: the cache copy and the command are the same
  // inode, so nothing was duplicated.
  const cache = path.join(s.home, "local-rag", "bin", targetTriple(KEY));
  assert.equal(
    fs.statSync(s.binOf("local-rag")).ino,
    fs.statSync(path.join(cache, executableName("local-rag", PLATFORM))).ino,
  );
});

test("a hard link taken before the install still holds the stub afterwards", async (t) => {
  // This is the shared-store hazard, reproduced. `writeFileSync` over the path
  // would rewrite the inode and every other name pointing at it — including a
  // package store's own copy, whose filename is the digest of what it used to
  // hold. `linkSync` + `renameSync` replaces the directory entry instead.
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({ tag: "3.3.0", assets });
  t.after(() => server.close());
  const s = scaffold("store");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));

  const store = path.join(s.root, "store");
  fs.mkdirSync(store, { recursive: true });
  const stubBytes = {};
  for (const binary of PRODUCT_BINARIES) {
    const shim = s.binOf(binary.name);
    stubBytes[binary.name] = fs.readFileSync(shim);
    fs.linkSync(shim, path.join(store, binary.name));
  }

  assert.equal((await runPostinstall(s, { LOCAL_RAG_RELEASE_BASE_URL: server.origin })).status, 0);

  for (const binary of PRODUCT_BINARIES) {
    assert.deepEqual(
      fs.readFileSync(path.join(store, binary.name)),
      stubBytes[binary.name],
      `${binary.name}: the store's copy must be untouched`,
    );
    assert.notDeepEqual(fs.readFileSync(s.binOf(binary.name)), stubBytes[binary.name]);
  }
});

test("a symlink created before the install now runs the native binary", async (t) => {
  // npm links `.bin` entries before it runs `postinstall` — verified in its own
  // `arborist/lib/arborist/rebuild.js`, where `#linkAllBins()` precedes
  // `#runScripts('postinstall')`. So the link is always older than the swap.
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({ tag: "3.3.0", assets });
  t.after(() => server.close());
  const s = scaffold("symlink");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));

  const dotBin = path.join(s.root, "node_modules", ".bin");
  fs.mkdirSync(dotBin, { recursive: true });
  const entry = path.join(dotBin, "local-rag-proxy");
  fs.symlinkSync(s.binOf("local-rag-proxy"), entry);

  assert.equal((await runPostinstall(s, { LOCAL_RAG_RELEASE_BASE_URL: server.origin })).status, 0);

  assert.equal(fs.lstatSync(entry).isSymbolicLink(), true, "the link itself is untouched");
  const out = execFileSync(entry, { encoding: "utf8" });
  assert.match(out, /NATIVE local-rag-proxy/, "and it execs the binary with no node in the way");
});

test("LOCAL_RAG_NO_BIN_REPLACE keeps the stub, and the stub still finds the binary", async (t) => {
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({ tag: "3.3.0", assets });
  t.after(() => server.close());
  const s = scaffold("disabled");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const before = fs.readFileSync(s.binOf("local-rag-proxy"));

  const result = await runPostinstall(s, {
    LOCAL_RAG_RELEASE_BASE_URL: server.origin,
    [DISABLE_VAR]: "1",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(fs.readFileSync(s.binOf("local-rag-proxy")), before, "still the stub");

  // And the install did happen, so the stub resolves it from the cache.
  const run = spawnSync(process.execPath, [s.binOf("local-rag-proxy")], {
    encoding: "utf8",
    env: {
      ...process.env,
      LOCAL_RAG_HOME: s.home,
      // Nothing to fetch: the cache is already populated, so this run never
      // reaches the network and `spawnSync` cannot deadlock against the server.
      LOCAL_RAG_RELEASE_BASE_URL: "http://127.0.0.1:1/releases",
    },
  });
  assert.equal(run.status, 0, run.stderr);
  assert.match(run.stdout, /NATIVE local-rag-proxy/);
});

test("a package manager that does not link bins as symlinks keeps the stub", async (t) => {
  // pnpm's default linker writes a `/bin/sh` wrapper with `exec node <target>`
  // baked in at creation time — and because bins are linked before install
  // scripts, that decision is made while the target is still the JS stub.
  // Replacing the target would leave the wrapper handing a native binary to
  // Node. Two shapes of that wrapper exist on this machine to compare: one for
  // a JS target says `exec node …`, one for a native target does not.
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({ tag: "3.3.0", assets });
  t.after(() => server.close());
  const s = scaffold("pnpm");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const before = fs.readFileSync(s.binOf("local-rag-proxy"));

  const result = await runPostinstall(s, {
    LOCAL_RAG_RELEASE_BASE_URL: server.origin,
    npm_config_user_agent: "pnpm/11.5.2 npm/? node/v24.18.1 darwin arm64",
  });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(fs.readFileSync(s.binOf("local-rag-proxy")), before);
  assert.match(result.stdout, /keeping the Node stubs: pnpm/);
});

test("running postinstall twice replaces nothing the second time", async (t) => {
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({ tag: "3.3.0", assets });
  t.after(() => server.close());
  const s = scaffold("idempotent");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));

  assert.equal((await runPostinstall(s, { LOCAL_RAG_RELEASE_BASE_URL: server.origin })).status, 0);
  const first = fs.statSync(s.binOf("local-rag")).ino;

  const again = await runPostinstall(s, { LOCAL_RAG_RELEASE_BASE_URL: server.origin });
  assert.equal(again.status, 0, again.stderr);
  assert.equal(fs.statSync(s.binOf("local-rag")).ino, first, "same inode, no churn");
  assert.doesNotMatch(again.stdout, /installed 4 binaries/, "and nothing was refetched");
});

test("postinstall exits 0 even when the release cannot be reached", async (t) => {
  // The package is still installed and every command still works — the stubs
  // heal on first use. Failing `npm install` over this would be a lie about
  // what happened and an obstruction to boot.
  const s = scaffold("offline");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const before = fs.readFileSync(s.binOf("local-rag-proxy"));

  const dead = "http://127.0.0.1:1/releases";
  const result = await runPostinstall(s, { LOCAL_RAG_RELEASE_BASE_URL: dead });
  assert.equal(result.status, 0, "postinstall never fails an install");
  assert.deepEqual(fs.readFileSync(s.binOf("local-rag-proxy")), before);
  assert.match(result.stderr, /the binaries are not/);
});
