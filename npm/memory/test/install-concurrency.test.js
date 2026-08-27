"use strict";

// Two installers on one directory (T22-08). The first three tests run real OS
// processes, the same tier `subprocess.test.js` occupies and for the same
// reason: mutual exclusion between two `require`d copies of a module in one
// process proves nothing about mutual exclusion between two `npm` runs.
//
// Every barrier here is a handshake, never a timer. The parent waits until a
// child *says* it is in the state the test needs — `LOCK acquired`, then
// `LOCK waiting` — so the race is arranged rather than hoped for. A test that
// slept and assumed would pass just as readily if the lock did nothing at all,
// which is the most expensive way this file could be wrong.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const crypto = require("node:crypto");
const { spawn } = require("node:child_process");

const { PRODUCT_BINARIES, assetName, sidecarName, executableName } = require("../src/release.js");
const { platformKey } = require("../src/platform.js");
const {
  acquireInstallLock,
  withInstallLock,
  lockPathFor,
  readLockRecord,
  probePid,
  LockError,
  LOCK_FILE,
} = require("../src/lock.js");
const { readManifest, installBinaries } = require("../src/install.js");
const { startFixtureRelease } = require("./helpers/fixture-server.js");
const { mkTmpRoot } = require("./helpers/tmp.js");
const { waitUntil, pidIsAlive, waitForStdoutLine } = require("./helpers/proc.js");
const { buildTar, buildZip } = require("./helpers/archive-fixtures.js");

const INSTALL_SCRIPT = path.join(__dirname, "..", "scripts", "install.js");

// The host's own key, unlike `install-verify.test.js`'s pinned `linux-x64`.
// A child process resolves its platform for itself and has no test seam to
// override it — deliberately, since a `--platform` flag would be production
// surface that exists only for tests. That costs nothing here: what these
// tests are about is the lock, and building the fixture for whatever platform
// the child will ask for exercises the real path rather than a stand-in.
const KEY = platformKey();
const PLATFORM = process.platform;

function sha256(buf) {
  return crypto.createHash("sha256").update(buf).digest("hex");
}

function releaseAssets() {
  const assets = {};
  for (const binary of PRODUCT_BINARIES) {
    const asset = assetName(binary.name, KEY);
    // The producer's own rule: `.zip` on Windows, `.tar.gz` everywhere else.
    const build = asset.endsWith(".zip") ? buildZip : buildTar;
    const archive = build([
      { name: executableName(binary.name, PLATFORM), data: `#!/bin/sh\necho ${binary.name}\n` },
    ]);
    assets[asset] = archive;
    assets[sidecarName(asset)] = `${sha256(archive)} *${asset}\n`;
  }
  return assets;
}

function spawnInstaller(dir, origin, extraArgs = []) {
  return spawn(process.execPath, [INSTALL_SCRIPT, "--dir", dir, ...extraArgs], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, LOCAL_RAG_RELEASE_BASE_URL: origin },
  });
}

function collect(stream) {
  const chunks = [];
  stream.on("data", (c) => chunks.push(c));
  return () => Buffer.concat(chunks).toString("utf8");
}

function exited(child) {
  return new Promise((resolve) => child.on("exit", (code) => resolve(code)));
}

/** Payload fetches only — the redirect that resolves the tag moves no bytes. */
function payloadRequests(server) {
  return server.requests().filter((r) => /^\/download\//.test(r.url));
}

test("two real installers on one directory: exactly one of them downloads", async (t) => {
  const assets = releaseAssets();
  const first = assetName(PRODUCT_BINARIES[0].name, KEY);
  // Two identical mirrors of one release, one per process. In production both
  // would reach the same host; here the split is what makes the request log
  // *attributable* — with one shared server, "the loser fetched nothing" is not
  // a question the log can answer, because the winner keeps fetching while the
  // loser waits. Nothing about the lock changes.
  const winnerServer = await startFixtureRelease({
    tag: "4.2.0",
    assets,
    // Holds the winner inside its first download long enough for the loser to
    // reach the lock. The barrier is still the loser's own "LOCK waiting" line;
    // this only makes the window exist.
    faults: { [first]: { trickleMs: 700 } },
  });
  t.after(() => winnerServer.close());
  const loserServer = await startFixtureRelease({ tag: "4.2.0", assets });
  t.after(() => loserServer.close());
  const dir = mkTmpRoot("lr-conc-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  const winner = spawnInstaller(dir, winnerServer.origin);
  t.after(() => winner.kill("SIGKILL"));
  const winnerErr = collect(winner.stderr);
  await waitForStdoutLine(winner, (l) => l === "LOCK acquired");

  const loser = spawnInstaller(dir, loserServer.origin);
  t.after(() => loser.kill("SIGKILL"));
  const loserOut = collect(loser.stdout);
  const loserErr = collect(loser.stderr);
  // Proof that the loser genuinely contended, rather than arriving after the
  // winner had already finished — which would pass every assertion below for
  // entirely the wrong reason.
  await waitForStdoutLine(loser, (l) => l === "LOCK waiting");

  assert.equal(await exited(winner), 0, winnerErr());
  assert.equal(await exited(loser), 0, loserErr());

  const expectedFetches = PRODUCT_BINARIES.length * 2; // one archive + one sidecar each
  assert.equal(
    payloadRequests(winnerServer).length,
    expectedFetches,
    "the winner fetched every asset exactly once",
  );
  assert.equal(
    payloadRequests(loserServer).length,
    0,
    "the loser fetched nothing at all — the double-check, not merely the lock",
  );
  assert.match(loserOut(), /up-to-date/);
  assert.equal(readManifest(dir).tag, "4.2.0");
  assert.equal(fs.existsSync(lockPathFor(dir)), false, "the lock is released by both");
});

test("a live lock makes the no-wait path exit at once and download nothing", async (t) => {
  const assets = releaseAssets();
  const server = await startFixtureRelease({ tag: "4.2.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-conc-nowait-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  // A lock held by this very test process, which is unambiguously alive.
  fs.writeFileSync(
    lockPathFor(dir),
    JSON.stringify({
      pid: process.pid,
      hostname: os.hostname(),
      token: "held-by-the-test",
      startedAt: Date.now(),
    }),
  );

  const child = spawnInstaller(dir, server.origin, ["--no-wait"]);
  t.after(() => child.kill("SIGKILL"));
  const err = collect(child.stderr);
  const code = await exited(child);

  assert.notEqual(code, 0, "a held lock is reported, not waited out");
  assert.match(err(), /another install is running/);
  assert.match(err(), new RegExp(`pid ${process.pid}`));
  assert.equal(payloadRequests(server).length, 0, "and nothing was fetched");
  assert.equal(
    readLockRecord(lockPathFor(dir)).token,
    "held-by-the-test",
    "the live lock was left exactly as it was",
  );
});

test("a lock left by a process that is provably gone is stolen", async (t) => {
  const assets = releaseAssets();
  const server = await startFixtureRelease({ tag: "4.2.0", assets });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-conc-stale-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  // A real pid whose death is observed rather than assumed.
  const corpse = spawn(process.execPath, ["-e", ""], { stdio: "ignore" });
  const corpsePid = corpse.pid;
  await exited(corpse);
  await waitUntil(() => !pidIsAlive(corpsePid), { description: `pid ${corpsePid} to be gone` });
  assert.equal(probePid(corpsePid), "dead");

  fs.writeFileSync(
    lockPathFor(dir),
    JSON.stringify({
      pid: corpsePid,
      hostname: os.hostname(),
      token: "left-by-a-corpse",
      startedAt: Date.now(),
    }),
  );

  const child = spawnInstaller(dir, server.origin);
  t.after(() => child.kill("SIGKILL"));
  const out = collect(child.stdout);
  const err = collect(child.stderr);

  assert.equal(await exited(child), 0, err());
  assert.match(out(), /LOCK stale/);
  assert.match(out(), /LOCK stole/);
  assert.equal(readManifest(dir).tag, "4.2.0");
  assert.equal(fs.existsSync(lockPathFor(dir)), false);
});

test("a lock that stopped being refreshed is stolen; a fresh one never is", async (t) => {
  const dir = mkTmpRoot("lr-conc-age-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const lockPath = lockPathFor(dir);

  // This process's own pid, so the liveness rule says "alive" and only the age
  // rule can fire. That is the accepted trade the module documents: a live
  // holder that has stopped heartbeating — suspended, SIGSTOP'd — is stolen
  // from. Its cost is a duplicate download, and the heartbeat is what makes the
  // case rare enough to accept.
  const record = {
    pid: process.pid,
    hostname: os.hostname(),
    token: "not-refreshed",
    startedAt: Date.now(),
  };

  fs.writeFileSync(lockPath, JSON.stringify(record));
  const fresh = await acquireInstallLock(dir, {
    wait: false,
    staleMs: 60_000,
    now: () => Date.now(),
  }).then(
    (g) => {
      g.release();
      return "acquired";
    },
    (err) => err.kind,
  );
  assert.equal(fresh, "held", "a lock refreshed a moment ago is never stolen");

  fs.writeFileSync(lockPath, JSON.stringify(record));
  const aged = Date.now() / 1000 - 3600;
  fs.utimesSync(lockPath, aged, aged);
  const guard = await acquireInstallLock(dir, { wait: false, staleMs: 60_000 });
  assert.equal(readLockRecord(lockPath).token, guard.token, "the stale lock was taken over");
  guard.release();
  assert.equal(fs.existsSync(lockPath), false);
});

test("a pid from another machine is never trusted; only age applies there", async (t) => {
  const dir = mkTmpRoot("lr-conc-host-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const lockPath = lockPathFor(dir);

  // A pid that is dead on *this* host, recorded by a different one. The number
  // means nothing across machines — or across a container's pid namespace — so
  // the liveness rule must not fire.
  fs.writeFileSync(
    lockPath,
    JSON.stringify({
      pid: 999_999,
      hostname: "some-other-box",
      token: "elsewhere",
      startedAt: Date.now(),
    }),
  );

  await assert.rejects(
    () => acquireInstallLock(dir, { wait: false, staleMs: 60_000 }),
    (err) => err instanceof LockError && err.kind === "held",
  );
  assert.equal(readLockRecord(lockPath).token, "elsewhere");
});

test("a departing owner never unlinks a record it no longer owns (D-084)", async (t) => {
  // The direct sibling of `daemon/lock.rs`'s
  // `a_departing_owner_never_unlinks_a_record_it_no_longer_owns`. There the
  // divergence became unreachable once reclaiming was removed; here reclaiming
  // is the whole design, so this is reachable by construction.
  const dir = mkTmpRoot("lr-conc-d084-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const lockPath = lockPathFor(dir);

  const guard = await acquireInstallLock(dir);
  assert.equal(readLockRecord(lockPath).token, guard.token);

  // Dispossess it exactly as a stealer would: the path now names a different
  // file, while the guard still holds a descriptor on the old inode.
  fs.unlinkSync(lockPath);
  const successor = {
    pid: process.pid,
    hostname: os.hostname(),
    token: "the-successor",
    startedAt: Date.now(),
  };
  fs.writeFileSync(lockPath, JSON.stringify(successor));

  assert.equal(guard.isOwned(), false, "the guard knows it was dispossessed");
  assert.throws(
    () => guard.assertOwned(),
    (err) => err instanceof LockError && err.kind === "lost",
  );

  guard.release();

  assert.equal(fs.existsSync(lockPath), true, "the successor's lock survives our release");
  assert.equal(readLockRecord(lockPath).token, "the-successor");
  fs.unlinkSync(lockPath);
});

test("the heartbeat refreshes our own inode and never a successor's", async (t) => {
  const dir = mkTmpRoot("lr-conc-beat-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const lockPath = lockPathFor(dir);

  const guard = await acquireInstallLock(dir, { heartbeatMs: 0 });
  const at = Date.now() + 90_000;
  guard.heartbeat(at);
  assert.equal(Math.round(fs.statSync(lockPath).mtimeMs), Math.round(at), "our own mtime moved");

  fs.unlinkSync(lockPath);
  fs.writeFileSync(lockPath, JSON.stringify({ pid: 1, hostname: "x", token: "s", startedAt: 0 }));
  const successorMtime = fs.statSync(lockPath).mtimeMs;
  // Through the descriptor, not the path — so a dispossessed holder cannot keep
  // its successor's lock looking alive.
  guard.heartbeat(Date.now() + 180_000);
  assert.equal(fs.statSync(lockPath).mtimeMs, successorMtime);

  guard.release();
  fs.rmSync(lockPath, { force: true });
});

test("one process cannot take the same lock twice", async (t) => {
  // A `--background` child spawned by a parent that already holds the lock
  // would otherwise wait out the budget and then steal from its own parent by
  // age, after which the parent's release would destroy the child's lock.
  const dir = mkTmpRoot("lr-conc-reentrant-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  await withInstallLock(dir, async () => {
    await assert.rejects(
      () => acquireInstallLock(dir, { wait: false }),
      (err) => err instanceof LockError && err.kind === "reentrant",
    );
  });
  // And the guard is gone from the register once released, so the next real
  // acquisition is not refused.
  const again = await acquireInstallLock(dir, { wait: false });
  again.release();
});

test("a symlink where the lock should be is stolen rather than looped on", async (t) => {
  // `open(path, "wx")` on a dangling symlink answers EEXIST while `stat`
  // answers ENOENT: anything that believes the second answer spins forever.
  const dir = mkTmpRoot("lr-conc-symlink-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const lockPath = lockPathFor(dir);
  fs.symlinkSync(path.join(dir, "nowhere"), lockPath);

  const guard = await acquireInstallLock(dir, { wait: false });
  assert.equal(fs.lstatSync(lockPath).isFile(), true, "a real lock replaced the symlink");
  guard.release();
});

test("an installer dispossessed mid-download refuses to publish a manifest", async (t) => {
  // The one way two holders are NOT harmless: under `latest` each resolves its
  // own tag, so a dispossessed process finishing its writes could publish a
  // manifest naming tag A over a set another installer has been filling with
  // tag B. `install.js` calls `assertOwned()` immediately before the manifest
  // write for exactly this, and nothing else in the suite reaches that line.
  const assets = releaseAssets();
  const first = assetName(PRODUCT_BINARIES[0].name, KEY);
  const server = await startFixtureRelease({
    tag: "4.2.0",
    assets,
    faults: { [first]: { trickleMs: 300 } },
  });
  t.after(() => server.close());
  const dir = mkTmpRoot("lr-conc-lost-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));

  const lockPath = lockPathFor(dir);
  const running = installBinaries(dir, {
    env: { LOCAL_RAG_RELEASE_BASE_URL: server.origin },
    key: KEY,
    platform: PLATFORM,
    packageVersion: "9.9.9",
    lock: { heartbeatMs: 0 },
  });

  // Take the lock away while the first download is stalled in the trickle.
  await waitUntil(() => fs.existsSync(lockPath), { description: "the lock to appear" });
  fs.unlinkSync(lockPath);
  fs.writeFileSync(
    lockPath,
    JSON.stringify({
      pid: process.pid,
      hostname: os.hostname(),
      token: "usurper",
      startedAt: Date.now(),
    }),
  );

  await assert.rejects(
    () => running,
    (err) => err instanceof LockError && err.kind === "lost",
  );
  const manifest = path.join(dir, ".local-rag-install.json");
  assert.equal(fs.existsSync(manifest), false, "nothing published");
  assert.equal(readLockRecord(lockPath).token, "usurper", "and the usurper's lock is untouched");
  fs.unlinkSync(lockPath);
});

test("a pid that names a process group is never read as a live holder", async (t) => {
  // `process.kill(0, 0)` signals our own process group and `process.kill(-1, 0)`
  // every process we may signal: neither throws, so a boolean liveness probe
  // reports "alive" for a record that names no process at all. The three-valued
  // answer is what keeps those out of the steal decision in both directions.
  assert.equal(probePid(0), "unknown");
  assert.equal(probePid(-1), "unknown");
  assert.equal(probePid(Number.NaN), "unknown");
  assert.equal(probePid(1.5), "unknown");
  assert.equal(probePid(process.pid), "alive");

  // The branch that cannot be reached with a real pid: `process.kill` answers
  // `ESRCH` or `EPERM` and nothing else, so an unfamiliar failure is only
  // observable through the injected probe. It must read as "unknown" — a
  // question we could not answer, never as evidence of death.
  const throwing = (code) => () => {
    const err = new Error(code);
    err.code = code;
    throw err;
  };
  assert.equal(probePid(4242, throwing("ESRCH")), "dead");
  assert.equal(probePid(4242, throwing("EPERM")), "alive");
  assert.equal(probePid(4242, throwing("EINVAL")), "unknown");
  assert.equal(probePid(4242, throwing("ERR_INVALID_ARG_TYPE")), "unknown");

  const dir = mkTmpRoot("lr-conc-pid0-");
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const lockPath = lockPathFor(dir);
  fs.writeFileSync(
    lockPath,
    JSON.stringify({ pid: 0, hostname: os.hostname(), token: "bad-pid", startedAt: Date.now() }),
  );
  // "unknown" licenses nothing: a fresh lock naming pid 0 is left alone, and
  // only the age rule can ever take it.
  await assert.rejects(
    () => acquireInstallLock(dir, { wait: false, staleMs: 60_000 }),
    (err) => err instanceof LockError && err.kind === "held",
  );
  assert.equal(readLockRecord(lockPath).token, "bad-pid");
});
