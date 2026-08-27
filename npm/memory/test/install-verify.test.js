"use strict";

// Installing, and refusing to install (T22-08). Everything here runs against a
// real HTTP fixture on loopback rather than a stubbed fetcher, so the client is
// covered by the same tests as the policy — the fetcher seam `install.rs` needs
// exists in Rust because `ureq` cannot be pointed at a local server as cheaply
// as `LOCAL_RAG_RELEASE_BASE_URL` can.
//
// The platform is pinned to `linux-x64` throughout rather than taken from the
// host, so the asset names, the archive format and the executable names are the
// same on every machine that runs this file.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");

const { PRODUCT_BINARIES, assetName, sidecarName, executableName } = require("../src/release.js");
const {
  installBinaries,
  readManifest,
  manifestIsCurrent,
  manifestPathFor,
  MANIFEST_FILE,
  TMP_PREFIX,
  ERROR_FILE,
  InstallError,
} = require("../src/install.js");
const { LOCK_FILE } = require("../src/lock.js");
const { startFixtureRelease } = require("./helpers/fixture-server.js");
const { mkTmpRoot } = require("./helpers/tmp.js");
const { buildTar } = require("./helpers/archive-fixtures.js");

const KEY = "linux-x64";
const PLATFORM = "linux";
const PKG_VERSION = "9.9.9";

function sha256(buf) {
  return crypto.createHash("sha256").update(buf).digest("hex");
}

/** A whole release: one archive plus one sidecar per binary. */
function releaseAssets({ omit = [] } = {}) {
  const assets = {};
  const payloads = {};
  for (const binary of PRODUCT_BINARIES) {
    if (omit.includes(binary.name)) continue;
    const asset = assetName(binary.name, KEY);
    const exe = executableName(binary.name, PLATFORM);
    const body = Buffer.from(`#!/bin/sh\nexec echo ${binary.name} "$@"\n`);
    const archive = buildTar([{ name: exe, data: body }]);
    assets[asset] = archive;
    assets[sidecarName(asset)] = `${sha256(archive)} *${asset}\n`;
    payloads[binary.name] = body;
  }
  return { assets, payloads };
}

function baseOptions(origin, overrides = {}) {
  return {
    env: { LOCAL_RAG_RELEASE_BASE_URL: origin },
    key: KEY,
    platform: PLATFORM,
    packageVersion: PKG_VERSION,
    ...overrides,
  };
}

/** Only the requests that moved a payload, not the redirect that found the tag. */
function downloads(server) {
  return server.requests().filter((r) => /^\/download\//.test(r.url));
}

function tempDirs(dir) {
  return fs.readdirSync(dir).filter((n) => n.startsWith(TMP_PREFIX));
}

test("a clean install places every binary, records the tag, and leaves no scratch", async (t) => {
  const { assets, payloads } = releaseAssets();
  const server = await startFixtureRelease({ tag: "2.1.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  const report = await installBinaries(dir, baseOptions(server.origin));

  assert.equal(report.skipped, false);
  assert.equal(report.tag, "2.1.0");
  assert.deepEqual(report.installed.sort(), PRODUCT_BINARIES.map((b) => b.name).sort());
  assert.deepEqual(report.absent, []);

  for (const binary of PRODUCT_BINARIES) {
    const file = path.join(dir, executableName(binary.name, PLATFORM));
    assert.deepEqual(fs.readFileSync(file), payloads[binary.name], binary.name);
    // The execute bit is the whole reason this is not 0600.
    assert.equal(fs.statSync(file).mode & 0o777, 0o755, binary.name);
  }

  const manifest = readManifest(dir);
  assert.equal(manifest.tag, "2.1.0");
  assert.equal(manifest.packageVersion, PKG_VERSION);
  assert.equal(manifest.platformKey, KEY);
  assert.equal(manifest.targetTriple, "x86_64-unknown-linux-gnu");
  assert.equal(manifest.binaries["local-rag"].state, "installed");

  assert.deepEqual(tempDirs(dir), [], "the per-attempt scratch directory must be gone");
  assert.equal(fs.existsSync(path.join(dir, LOCK_FILE)), false, "the lock must be released");
  assert.equal(fs.existsSync(path.join(dir, ERROR_FILE)), false);
});

test("the manifest is written last: a failure on the final asset leaves none", async (t) => {
  // The strongest available proof that nothing is published incrementally —
  // three binaries are durably on disk and the directory still reads as "not
  // installed", which is the property the whole ordering exists for.
  const last = PRODUCT_BINARIES[PRODUCT_BINARIES.length - 1];
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({
    tag: "2.1.0",
    assets,
    faults: { [assetName(last.name, KEY)]: { status: 500 } },
  });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-last-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  // The optional binary is last, so make it required for this run.
  const binaries = PRODUCT_BINARIES.map((b) => ({ ...b, required: true }));
  await assert.rejects(
    () => installBinaries(dir, baseOptions(server.origin, { binaries })),
    (err) => err instanceof InstallError,
  );

  for (const binary of PRODUCT_BINARIES.slice(0, -1)) {
    assert.ok(
      fs.existsSync(path.join(dir, executableName(binary.name, PLATFORM))),
      `${binary.name} was placed before the failure`,
    );
  }
  assert.equal(fs.existsSync(manifestPathFor(dir)), false, "no manifest");
  assert.equal(readManifest(dir), null);
  const expected = { packageVersion: PKG_VERSION, key: KEY };
  assert.equal(manifestIsCurrent(readManifest(dir), expected, dir), false);
  assert.deepEqual(tempDirs(dir), []);
});

test("a digest that does not match installs nothing and names both digests", async (t) => {
  const { assets } = releaseAssets();
  const target = assetName("local-rag", KEY);
  const wrong = "0".repeat(64);
  assets[sidecarName(target)] = `${wrong} *${target}\n`;
  const server = await startFixtureRelease({ tag: "2.1.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-digest-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  await assert.rejects(
    () => installBinaries(dir, baseOptions(server.origin)),
    (err) => {
      assert.ok(err instanceof InstallError && err.kind === "checksum");
      assert.match(err.message, new RegExp(wrong));
      assert.match(err.message, /actual\s+[0-9a-f]{64}/);
      return true;
    },
  );

  assert.equal(fs.existsSync(path.join(dir, "local-rag")), false, "nothing installed");
  assert.equal(fs.existsSync(manifestPathFor(dir)), false, "no manifest");
  assert.deepEqual(tempDirs(dir), [], "no .part left anywhere");
  // The failure is legible afterwards even when stdio went nowhere.
  assert.match(fs.readFileSync(path.join(dir, ERROR_FILE), "utf8"), /checksum|does not match/);
});

test("an interruption is healed by running again, and the third run is a no-op", async (t) => {
  const third = PRODUCT_BINARIES[2];
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({
    tag: "2.1.0",
    assets,
    // Unavailable on the first attempt, present on the second. A 404 rather
    // than a 5xx on purpose: `http.js` retries 5xx and would heal it inside a
    // single run, whereas a 404 is "an answer, not a glitch" and is not
    // retried — which is what makes it a stand-in for a run that simply died
    // partway. What is under test is the installer's resume, not the retry
    // policy T22-06 already covers.
    faults: { [assetName(third.name, KEY)]: { status: 404, failTimes: 1 } },
  });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-resume-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  await assert.rejects(() => installBinaries(dir, baseOptions(server.origin)));
  assert.equal(fs.existsSync(manifestPathFor(dir)), false, "an interrupted install is absent");

  const second = await installBinaries(dir, baseOptions(server.origin));
  assert.equal(second.skipped, false);
  assert.equal(readManifest(dir).tag, "2.1.0");

  const before = downloads(server).length;
  const third_ = await installBinaries(dir, baseOptions(server.origin, { mode: "if-needed" }));
  assert.equal(third_.skipped, true);
  assert.equal(third_.reason, "already-installed");
  assert.equal(downloads(server).length, before, "a satisfied if-needed run asks for nothing");
});

test("an optional asset missing from the release is recorded, not fatal", async (t) => {
  // `local-rag-tui` is absent from tag 0.0.0 because it postdates the crate.
  // Under `latest` the tag can always be older than the package.
  const { assets } = releaseAssets({ omit: ["local-rag-tui"] });
  const server = await startFixtureRelease({ tag: "0.0.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-optional-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  const report = await installBinaries(dir, baseOptions(server.origin));

  assert.equal(report.skipped, false);
  assert.deepEqual(report.absent, ["local-rag-tui"]);
  assert.equal(readManifest(dir).binaries["local-rag-tui"].state, "absent");
  assert.equal(fs.existsSync(path.join(dir, "local-rag-tui")), false);
  assert.ok(fs.existsSync(path.join(dir, "local-rag-proxy")));
});

test("a required asset missing from the release is fatal, and nothing is published", async (t) => {
  const { assets } = releaseAssets({ omit: ["local-rag-proxy"] });
  const server = await startFixtureRelease({ tag: "0.0.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-required-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  await assert.rejects(
    () => installBinaries(dir, baseOptions(server.origin)),
    (err) => err instanceof InstallError && err.kind === "asset-absent",
  );
  assert.equal(fs.existsSync(manifestPathFor(dir)), false);
});

test("the sidecar defines whether an asset is there, so a lone archive is loud", async (t) => {
  // A release carrying an archive but no checksum is broken, not incomplete:
  // ADR-0013 forbids installing what cannot be verified, so this must not be
  // quietly treated as "absent".
  const { assets } = releaseAssets();
  delete assets[sidecarName(assetName("local-rag-tui", KEY))];
  const server = await startFixtureRelease({ tag: "2.1.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-nosidecar-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  const report = await installBinaries(dir, baseOptions(server.origin));
  // `local-rag-tui` is optional, so the run survives — but it is recorded as
  // absent rather than installed from an unverifiable archive.
  assert.deepEqual(report.absent, ["local-rag-tui"]);
  assert.equal(fs.existsSync(path.join(dir, "local-rag-tui")), false);
});

test("a manifest that is malformed, foreign or stale reads as not installed", async (t) => {
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({ tag: "2.1.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-manifest-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  await installBinaries(dir, baseOptions(server.origin));
  const good = readManifest(dir);
  const expected = { packageVersion: PKG_VERSION, key: KEY };

  assert.equal(manifestIsCurrent(good, expected, dir), true);

  fs.writeFileSync(manifestPathFor(dir), "{ not json");
  assert.equal(readManifest(dir), null, "unparseable is null, not a throw");
  assert.equal(manifestIsCurrent(readManifest(dir), expected, dir), false);

  fs.writeFileSync(manifestPathFor(dir), JSON.stringify({ ...good, packageVersion: "0.0.1" }));
  assert.equal(manifestIsCurrent(readManifest(dir), expected, dir), false, "a foreign wrapper");

  fs.writeFileSync(manifestPathFor(dir), JSON.stringify({ ...good, manifestVersion: 99 }));
  assert.equal(manifestIsCurrent(readManifest(dir), expected, dir), false, "a future schema");

  fs.writeFileSync(manifestPathFor(dir), JSON.stringify({ ...good, platformKey: "darwin-arm64" }));
  assert.equal(manifestIsCurrent(readManifest(dir), expected, dir), false, "another platform");

  // A manifest that claims a file which is not there is not current either.
  fs.writeFileSync(manifestPathFor(dir), JSON.stringify(good));
  fs.rmSync(path.join(dir, "local-rag-hook"));
  assert.equal(manifestIsCurrent(readManifest(dir), expected, dir), false, "a deleted binary");
});

test("a moved tag reinstalls, and an unmoved one does not", async (t) => {
  const { assets } = releaseAssets();
  const first = await startFixtureRelease({ tag: "2.1.0", assets });
  t.after(() => first.close());
  const dir = mkTmpRoot("lr-install-tag-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  await installBinaries(dir, baseOptions(first.origin));
  const again = await installBinaries(dir, baseOptions(first.origin));
  assert.equal(again.skipped, true);
  assert.equal(again.reason, "up-to-date");

  const moved = await startFixtureRelease({ tag: "3.0.0", assets });
  t.after(() => moved.close());
  const after = await installBinaries(dir, baseOptions(moved.origin));
  assert.equal(after.skipped, false, "a tag that moved is what makes an update happen");
  assert.equal(readManifest(dir).tag, "3.0.0");
});

test("--force reinstalls a directory that is already current", async (t) => {
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({ tag: "2.1.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-force-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  await installBinaries(dir, baseOptions(server.origin));
  const before = downloads(server).length;
  const forced = await installBinaries(dir, baseOptions(server.origin, { mode: "force" }));
  assert.equal(forced.skipped, false);
  assert.ok(downloads(server).length > before, "force actually refetches");
});

test("an unsupported platform is refused before anything is written", async (t) => {
  const dir = mkTmpRoot("lr-install-unsupported-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  await assert.rejects(
    () => installBinaries(dir, baseOptions("http://127.0.0.1:1", { key: "win32-arm64" })),
    (err) => err instanceof InstallError && err.kind === "unsupported-platform",
  );
  assert.deepEqual(fs.readdirSync(dir), [], "nothing was created");
});

test("a stale scratch directory is swept, a fresh one is left alone", async (t) => {
  const { assets } = releaseAssets();
  const server = await startFixtureRelease({ tag: "2.1.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-install-sweep-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  const stale = path.join(dir, `${TMP_PREFIX}dead`);
  const fresh = path.join(dir, `${TMP_PREFIX}alive`);
  fs.mkdirSync(stale, { recursive: true });
  fs.mkdirSync(fresh, { recursive: true });
  const old = Date.now() / 1000 - 3600;
  fs.utimesSync(stale, old, old);

  await installBinaries(dir, baseOptions(server.origin, { staleMs: 60_000 }));

  assert.equal(fs.existsSync(stale), false, "a scratch directory nobody refreshed is swept");
  assert.equal(
    fs.existsSync(fresh),
    true,
    "a fresh one belongs to a live attempt and must survive",
  );
});
