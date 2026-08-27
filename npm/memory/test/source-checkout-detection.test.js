"use strict";

// Recognising this package's own source checkout (T22-09).
//
// The owner's requirement is that `pnpm link --global` from the repository
// makes the locally built binaries usable — with no network and no
// `postinstall`, because pnpm does not run lifecycle scripts for linked
// packages. So detection has to happen at run time, from the package's own
// location, and it has to be right in both directions: a real checkout must be
// found, and a directory that merely looks like one must not be.
//
// Every tree here is synthetic. Anchoring on the real package would make the
// positive cases pass on a developer's machine for reasons that have nothing to
// do with this code — this checkout already has a global npm link pointing at
// it and hand-made symlinks from `npm/memory-darwin-arm64/bin` into
// `target/release`.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const { PRODUCT_BINARIES, executableName } = require("../src/release.js");
const { targetTriple } = require("../src/platform.js");
const { MANIFEST_FILE, MANIFEST_VERSION } = require("../src/install.js");
const { locateBinDir, sourceCheckoutRoot, BIN_DIR_VAR } = require("../src/locate.js");
const { writeLauncherPackageAt } = require("./helpers/fixture-layout.js");
const { startFixtureRelease } = require("./helpers/fixture-server.js");
const { mkTmpRoot } = require("./helpers/tmp.js");

const KEY = "linux-x64";
const PLATFORM = "linux";
const PKG_VERSION = "7.7.7";
const REQUIRED = PRODUCT_BINARIES.filter((b) => b.required).map((b) => b.name);

function writeBinaries(dir, names) {
  fs.mkdirSync(dir, { recursive: true });
  for (const name of names) {
    const file = path.join(dir, executableName(name, PLATFORM));
    fs.writeFileSync(file, `#!/bin/sh\nexec echo ${name}\n`);
    fs.chmodSync(file, 0o755);
  }
  return dir;
}

/**
 * A tree shaped like this repository. `writeLauncherPackageAt` copies the real
 * `src/`, `bin/` and `package.json`, so the fixture is the package rather than
 * a stand-in for it.
 */
function buildCheckout(root, { markers = true, profiles = ["release"] } = {}) {
  const packageDir = path.join(root, "npm", "memory");
  fs.mkdirSync(packageDir, { recursive: true });
  writeLauncherPackageAt(packageDir);
  if (markers) {
    const workspace = '[workspace]\nmembers = ["crates/core"]\n';
    fs.writeFileSync(path.join(root, "Cargo.toml"), workspace);
    fs.writeFileSync(path.join(root, "dist-workspace.toml"), '[dist]\ntargets = []\n');
  }
  for (const profile of profiles) {
    writeBinaries(path.join(root, "target", profile), REQUIRED);
  }
  return packageDir;
}

/** A per-user cache that would resolve, so "terminal" means something. */
function writeCache(home) {
  const dir = path.join(home, "local-rag", "bin", targetTriple(KEY));
  writeBinaries(dir, REQUIRED);
  const binaries = {};
  for (const b of PRODUCT_BINARIES) {
    binaries[b.name] = REQUIRED.includes(b.name)
      ? { state: "installed", file: executableName(b.name, PLATFORM) }
      : { state: "absent" };
  }
  fs.writeFileSync(
    path.join(dir, MANIFEST_FILE),
    JSON.stringify({
      manifestVersion: MANIFEST_VERSION,
      packageVersion: PKG_VERSION,
      platformKey: KEY,
      targetTriple: targetTriple(KEY),
      tag: "9.9.9",
      binaries,
    }),
  );
  return dir;
}

function opts(packageDir, home, extra = {}) {
  return {
    env: { LOCAL_RAG_HOME: home, ...(extra.env ?? {}) },
    key: KEY,
    platform: PLATFORM,
    packageDir,
    packageVersion: PKG_VERSION,
    ...extra,
  };
}

test("a checkout resolves its own target/release", (t) => {
  const root = mkTmpRoot("lr-checkout-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageDir = buildCheckout(root);

  assert.equal(sourceCheckoutRoot(packageDir), root);
  const r = locateBinDir(opts(packageDir, path.join(root, "home")));
  assert.equal(r.ok, true);
  assert.equal(r.source, "checkout");
  assert.equal(r.dir, path.join(root, "target", "release"));
  assert.equal(r.repoRoot, root);
});

test("release beats debug, and debug is taken when it is all there is", (t) => {
  const root = mkTmpRoot("lr-checkout-profiles-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageDir = buildCheckout(root, { profiles: ["release", "debug"] });
  const home = path.join(root, "home");

  assert.equal(
    locateBinDir(opts(packageDir, home)).dir,
    path.join(root, "target", "release"),
  );

  fs.rmSync(path.join(root, "target", "release"), { recursive: true, force: true });
  const fallback = locateBinDir(opts(packageDir, home));
  assert.equal(fallback.source, "checkout");
  assert.equal(fallback.dir, path.join(root, "target", "debug"));
});

test("a checkout with nothing built says so, and does not reach for a download", (t) => {
  // The rung below would resolve. Taking it would run a release from some other
  // day against source that is sitting right there.
  const root = mkTmpRoot("lr-checkout-unbuilt-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageDir = buildCheckout(root, { profiles: [] });
  const home = path.join(root, "home");
  writeCache(home);

  const r = locateBinDir(opts(packageDir, home));
  assert.equal(r.ok, false);
  assert.equal(r.reason, "checkout-not-built");
  assert.match(r.message, /cargo build --release -p local-rag/);
  assert.match(r.message, /the local build is the point/);
  assert.ok(
    r.candidates.every((c) => c.source === "checkout"),
    "the cache was never even consulted",
  );
});

test("a checkout beats an installed cache, and LOCAL_RAG_BIN_DIR beats the checkout", (t) => {
  const root = mkTmpRoot("lr-checkout-order-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageDir = buildCheckout(root);
  const home = path.join(root, "home");
  writeCache(home);

  assert.equal(locateBinDir(opts(packageDir, home)).source, "checkout");

  const override = writeBinaries(path.join(root, "vendored"), REQUIRED);
  const overridden = locateBinDir(opts(packageDir, home, { env: { [BIN_DIR_VAR]: override } }));
  assert.equal(overridden.source, "override");
  assert.equal(overridden.dir, override);
});

test("a tree with the right shape but no markers is not a checkout", (t) => {
  // `npm/memory` two levels down is not rare. Without `Cargo.toml` and
  // `dist-workspace.toml` there is no reason to believe a `target/` next to it
  // holds this project's binaries.
  const root = mkTmpRoot("lr-checkout-nomarkers-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageDir = buildCheckout(root, { markers: false });
  const home = path.join(root, "home");
  const cache = writeCache(home);

  assert.equal(sourceCheckoutRoot(packageDir), null);
  const r = locateBinDir(opts(packageDir, home));
  assert.equal(r.source, "cache");
  assert.equal(r.dir, cache);
});

test("a package that is not the tree's own npm/memory does not adopt that tree", (t) => {
  // The identity half. A copy of this package placed inside somebody else's
  // checkout sits at the same depth and sees the same markers; only comparing
  // the real paths tells them apart.
  const root = mkTmpRoot("lr-checkout-identity-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  buildCheckout(root);

  const foreign = path.join(root, "vendor", "memory");
  fs.mkdirSync(foreign, { recursive: true });
  writeLauncherPackageAt(foreign);
  // Two levels up from `<root>/vendor/memory` is `<root>` — markers and all.
  assert.equal(path.resolve(foreign, "..", ".."), root);
  assert.equal(sourceCheckoutRoot(foreign), null, "same shape, different package");

  const home = path.join(root, "home");
  const cache = writeCache(home);
  assert.equal(locateBinDir(opts(foreign, home)).dir, cache);
});

test("a symlinked package directory resolves the same checkout", (t) => {
  // The `pnpm link --global` shape: the entry a package manager exposes is a
  // symlink into the working tree.
  const root = mkTmpRoot("lr-checkout-link-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageDir = buildCheckout(root);
  const store = path.join(root, "global-store");
  fs.mkdirSync(store, { recursive: true });
  const link = path.join(store, "memory");
  fs.symlinkSync(packageDir, link);
  assert.equal(fs.lstatSync(link).isSymbolicLink(), true, "the fixture is really a symlink");

  assert.equal(sourceCheckoutRoot(link), root);
  const r = locateBinDir(opts(link, path.join(root, "home")));
  assert.equal(r.source, "checkout");
  assert.equal(r.dir, path.join(root, "target", "release"));
});

test("a checkout under a path with spaces and parentheses resolves", (t) => {
  // Carried over from `spaces-and-symlinks.test.js`, whose subject retires with
  // the platform packages: nothing else covers it, and this repository's own
  // path has no spaces in it to catch a regression by accident.
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "lr locate (spaces)-")));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageDir = buildCheckout(root);

  assert.match(root, /\s/);
  assert.equal(sourceCheckoutRoot(packageDir), root);
  const r = locateBinDir(opts(packageDir, path.join(root, "home")));
  assert.equal(r.source, "checkout");
  assert.equal(r.dir, path.join(root, "target", "release"));
});

test("resolving a checkout touches no network at all", async (t) => {
  // Not an inference from reading the code: a real server is listening, its
  // base URL is in the environment resolution runs under, and it must record
  // nothing.
  const server = await startFixtureRelease({ tag: "9.9.9", assets: {} });
  t.after(() => server.close());
  const root = mkTmpRoot("lr-checkout-offline-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const packageDir = buildCheckout(root);

  const r = locateBinDir(
    opts(packageDir, path.join(root, "home"), {
      env: { LOCAL_RAG_RELEASE_BASE_URL: server.origin },
    }),
  );
  assert.equal(r.source, "checkout");
  assert.equal(server.requestCount(), 0);
});

test("a missing or unreadable package directory is null, not a throw", (t) => {
  assert.equal(sourceCheckoutRoot(path.join(os.tmpdir(), "lr-does-not-exist-at-all")), null);
});
