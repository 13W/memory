"use strict";

// Mutual exclusion for the installer, and an explicit account of what it is
// and is not for.
//
// WHAT THE LOCK BUYS. Not correctness. Correctness lives in `install.js`'s
// ordering — `.part` → fsync → verify → rename → the manifest last — exactly as
// it does in `crates/models/src/install.rs`, which has no lock at all and is
// safe anyway. What this buys is not downloading a hundred megabytes twice.
// That distinction decides most trade-offs below: when a rule here is wrong,
// the cost is usually bandwidth rather than a corrupt installation.
//
// "USUALLY" IS DOING WORK IN THAT SENTENCE, so here is the exception. Two
// holders installing at once is harmless in every step but one: they write into
// per-attempt temporary directories that cannot collide, and they rename
// byte-identical verified files over the same targets. But under `latest` each
// resolves its own tag, so if a release is cut between them, one can publish a
// manifest naming tag A over a set of files the other has been filling with
// tag B. That is a genuinely torn install, and no amount of atomic renaming
// catches it, because every individual step was atomic and correct. It is
// closed by `assertOwned()`: `install.js` calls it immediately before writing
// the manifest, so a process that has been dispossessed refuses to publish.
//
// D-084's LESSON TRANSFERS ONLY HALF, AND THE OTHER HALF INVERTS.
//
// `crates/local-rag/src/daemon/lock.rs` refuses to reclaim a held lock for any
// reason at all. Its argument is precise: the lock is a `flock`, the OS drops a
// `flock` when the holding process dies, so reaching the "already held" branch
// *proves* a live holder — "nothing found there can prove the owner dead — not
// an unreadable record, not a mismatched pid, not a socket that will not
// answer."
//
// That premise is false here. `fs.openSync(path, "wx")` leaves a file behind
// when the process dies, and no kernel takes it away. So the existence of this
// lock file proves nothing about anybody being alive, and applying "never
// reclaim" would wedge the installer permanently after a single Ctrl-C in a
// `postinstall`. The conclusion inverts with the premise: a liveness test is
// not forbidden here, it is mandatory.
//
// What survives is the *derivation*, and it is a hard constraint on the steal
// predicate: NEVER STEAL ON EVIDENCE A LIVE HOLDER CAN LEGITIMATELY PRODUCE.
// D-084's socket probe was not wrong because probes are wrong; it was wrong
// because a shutting-down-but-alive daemon legitimately produces "the socket
// does not answer". Run this module's two rules through that test:
//
//   - an unreadable record: a live holder produces one, for the microseconds
//     between `open(wx)` and `writeSync`. NOT admissible — it is also exactly
//     D-065. An unreadable record is stealable by age alone, never by pid.
//   - a pid that is gone, on this host: a live holder cannot produce that.
//     Admissible.
//   - an age past the budget, WITHOUT a heartbeat: a live holder produces one
//     every time a 30 MB download crosses a slow link. NOT admissible.
//   - an age past the budget, WITH a heartbeat: only a stopped or wedged
//     process produces that. Admissible.
//
// So the heartbeat is not a refinement of the age rule. It is what makes the
// age rule admissible at all, and without it "steal by age" would be D-084's
// mistake wearing a different hat. It also decouples the two numbers that a
// naive age limit fuses: how slow a legitimate install may be (unbounded) and
// how long a crashed one wedges the next attempt (one budget).
//
// The second half of D-084 transfers verbatim, and matters more here than
// there. `StoreLockGuard::release` unlinks only when the path still names its
// own inode, because "anything that unlinks and recreates `store.lock` while
// this guard is alive leaves us holding a lock on an inode the path no longer
// names. Unlinking by path then deletes a *live* successor's record" — D-084's
// third daemon. There, that became unreachable once reclaiming was removed.
// Here reclaiming is the whole point, so it is reachable by construction.
//
// AND THE CHECK NEEDS A GUARANTEE THE RUST GETS FOR FREE. `still_owns_path`
// compares `self.file.metadata()` against `fs::metadata(path)`, and `self.file`
// is a *live open handle*. That is not incidental: POSIX will not recycle an
// inode number while a descriptor still references it, which is what makes
// `ino` an identity rather than a reusable integer. A port that closed the
// descriptor and remembered a number would inherit the check without the
// guarantee — and unlink-then-create in one directory lands on the same inode
// routinely on ext4 and tmpfs, while APFS effectively never recycles. The
// hazard would therefore be invisible on a Mac and ordinary on Linux CI. So
// the descriptor is held for the whole lifetime of the lock, and the token
// below closes what is left.
//
// KNOWN LIMITS, stated rather than defended. The age rule subtracts the
// filesystem's `mtime` from this process's clock; on a network filesystem those
// are two clocks and the difference is the skew. A negative age is clamped to
// zero so the "server ahead" direction cannot wedge us forever; the "server
// behind" direction costs duplicate downloads, not corruption. Nothing here is
// verified on Windows or on NFS — there is no such host in this environment,
// the same class of gap D-029 already records for ORT bundling.

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const crypto = require("node:crypto");

const LOCK_FILE = ".local-rag-install.lock";

// Only has to outlast a few missed heartbeats, which is what the heartbeat
// buys: it no longer has to outlast the slowest legitimate install.
const DEFAULT_STALE_MS = 60_000;
const DEFAULT_HEARTBEAT_MS = 5_000;
// Chosen, not derived — the same honesty `LOCK_HANDOVER_BUDGET_MS` states. Long
// enough to outlast a sibling that is nearly finished, short enough that a
// wedged install is a minute's wait inside `npm install` rather than a hang.
const DEFAULT_WAIT_MS = 60_000;
const DEFAULT_POLL_MS = 50;
// A lock record is one small JSON object. Anything larger is not one of ours.
const MAX_RECORD_BYTES = 4096;

/** Errors carry a kind so "someone else has it" is not "this is broken". */
class LockError extends Error {
  /** @param {"held"|"io"|"reentrant"|"lost"} kind */
  constructor(kind, message, { lockPath, owner } = {}) {
    super(message);
    this.name = "LockError";
    this.kind = kind;
    this.lockPath = lockPath;
    this.owner = owner ?? null;
  }
}

/** @param {string} dir @returns {string} */
function lockPathFor(dir) {
  return path.join(dir, LOCK_FILE);
}

// One `withInstallLock` per directory per process. Without this, a
// `--background` child spawned by a parent that already holds the lock waits
// out the budget and then steals from its own parent by age, after which the
// parent's release destroys the child's lock. Keyed by realpath because
// `/var` and `/private/var` name one directory on macOS and pnpm's store is
// symlinks all the way down — `test/helpers/tmp.js` documents the same trap.
const heldInThisProcess = new Map();

function processKey(dir) {
  try {
    return fs.realpathSync(dir);
  } catch {
    return path.resolve(dir);
  }
}

/**
 * The record inside a lock file, or null when there is none to read.
 *
 * Never throws: an unreadable or malformed record is a fact about the lock, not
 * an error in the reader. `lstat` first so a symlink planted at the path is
 * never opened, and the read is capped — a lock record is a few hundred bytes
 * and anything bigger is not one.
 *
 * @param {string} lockPath
 * @returns {{pid: number, hostname: string, token: string, startedAt: number}|null}
 */
function readLockRecord(lockPath) {
  let stat;
  try {
    stat = fs.lstatSync(lockPath);
  } catch {
    return null;
  }
  if (!stat.isFile() || stat.size > MAX_RECORD_BYTES) return null;
  let text;
  try {
    text = fs.readFileSync(lockPath, "utf8");
  } catch {
    return null;
  }
  try {
    const parsed = JSON.parse(text);
    if (parsed === null || typeof parsed !== "object") return null;
    return parsed;
  } catch {
    return null;
  }
}

/**
 * Three-valued, and the third value is the point.
 *
 * `"dead"` is the only answer that licenses a steal. `process.kill` says
 * `ESRCH` for a pid that is gone and `EPERM` for a live one owned by somebody
 * else — but it also does not throw at all for `0` (our own process group) or
 * `-1` (every process), and throws `ERR_INVALID_ARG_TYPE` for a pid that is not
 * an integer. A boolean collapses those into one of the two real answers, and
 * whichever way it collapses is wrong: `test/helpers/proc.js`'s `pidIsAlive`
 * folds them into "not alive", which is correct for waiting on a child and
 * would license a steal here.
 *
 * @param {unknown} pid
 * @param {(pid: number, signal: number) => void} [kill] injected so the last
 *   branch below is reachable from a test. It is otherwise unreachable in
 *   practice — a validated integer pid draws `ESRCH` or `EPERM` and nothing
 *   else — and defensive code nobody can execute is decoration rather than
 *   defence.
 * @returns {"alive"|"dead"|"unknown"}
 */
function probePid(pid, kill = (p, signal) => process.kill(p, signal)) {
  if (!Number.isInteger(pid) || pid <= 0 || pid > 2 ** 31 - 1) return "unknown";
  try {
    kill(pid, 0);
    return "alive";
  } catch (err) {
    if (err && err.code === "ESRCH") return "dead";
    if (err && err.code === "EPERM") return "alive";
    // Anything else is a question we could not answer, and an unanswered
    // question must never license a steal.
    return "unknown";
  }
}

/**
 * Move `lockPath` aside, confirm it was the file we judged, and delete it —
 * putting it back if it was not.
 *
 * `rename` rather than `unlink`, for three reasons in order of weight. It gives
 * a discriminator: the loser of a two-way race gets `ENOENT`, so exactly one
 * process performs the destructive act per corpse. It is verifiable after the
 * fact: `stat` on the moved-aside name says which inode we actually took, and
 * `unlink` can be asked nothing at all. And on Windows, unlinking a file
 * another process holds open leaves the *name* in a delete-pending state that
 * fails a subsequent create, where `rename` frees the name at once.
 *
 * Winning the rename never grants the lock. The claim is always the `wx` open,
 * in exactly one place, so "can two processes hold it?" has one answer — no, by
 * `O_EXCL` — instead of two primitives whose interaction needs reasoning about.
 *
 * @returns {"removed"|"gone"|"restored"|"lost-foreign"|"io"}
 */
function removeIfOurs(lockPath, expected, token) {
  const aside = `${lockPath}.rm-${token}`;
  try {
    fs.renameSync(lockPath, aside);
  } catch (err) {
    if (err && err.code === "ENOENT") return "gone";
    return "io";
  }
  let moved = null;
  try {
    moved = fs.statSync(aside);
  } catch {
    moved = null;
  }
  if (moved && moved.dev === expected.dev && moved.ino === expected.ino) {
    try {
      fs.rmSync(aside, { force: true, recursive: true });
    } catch {
      // Litter, swept later while holding the lock.
    }
    return "removed";
  }
  // We moved something that is not what we judged. `link` never clobbers, so
  // putting it back either restores the successor exactly or tells us the name
  // has already been taken by a third party.
  try {
    fs.linkSync(aside, lockPath);
    fs.rmSync(aside, { force: true, recursive: true });
    return "restored";
  } catch {
    try {
      fs.rmSync(aside, { force: true, recursive: true });
    } catch {
      // Nothing further to do.
    }
    return "lost-foreign";
  }
}

/** Remove `.rm-*`/`.probe-*` leftovers older than the stale budget. */
function sweepLockLitter(dir, staleMs, now) {
  let entries;
  try {
    entries = fs.readdirSync(dir);
  } catch {
    return;
  }
  for (const name of entries) {
    if (!name.startsWith(`${LOCK_FILE}.rm-`)) continue;
    const full = path.join(dir, name);
    try {
      if (now - fs.statSync(full).mtimeMs > staleMs) {
        fs.rmSync(full, { force: true, recursive: true });
      }
    } catch {
      // Gone already, or not ours to remove.
    }
  }
}

/**
 * A held lock. `heartbeat` keeps it from ageing out; `assertOwned` is what a
 * caller puts in front of any step that must not happen after dispossession.
 */
class LockGuard {
  constructor(lockPath, fd, token, opts, processKey) {
    this.lockPath = lockPath;
    this.fd = fd;
    this.token = token;
    this.opts = opts;
    this.processKey = processKey;
    this.released = false;
    this.timer = null;
  }

  /** Refresh the mtime through our own descriptor, never by path. */
  heartbeat(atMs = this.opts.now()) {
    if (this.released) return;
    try {
      const seconds = atMs / 1000;
      fs.futimesSync(this.fd, seconds, seconds);
    } catch {
      // Best-effort: a missed refresh costs at worst an early steal, which
      // costs at worst a duplicate download.
    }
  }

  startHeartbeat() {
    if (this.opts.heartbeatMs <= 0) return;
    this.timer = setInterval(() => {
      this.heartbeat();
      if (!this.isOwned()) this.opts.onEvent("lost", { lockPath: this.lockPath });
    }, this.opts.heartbeatMs);
    // Never keep a process alive on an abandoned guard.
    if (typeof this.timer.unref === "function") this.timer.unref();
  }

  /**
   * Whether `lockPath` still names the very file this guard holds open.
   *
   * D-084's second half, ported — and deliberately stronger than the original
   * off POSIX. The Rust answers `true` unconditionally under
   * `#[cfg(not(unix))]`, i.e. unlinks unconditionally, which is tolerable there
   * because Windows is not a supported daemon target. It is a first-class
   * install target here, and a token needs no filesystem support at all, so
   * win32 gets the token check rather than a blanket yes.
   *
   * Fail-closed throughout: any error, a short read, a record that will not
   * parse, all count as not-ours. Leaving a stale lock behind costs a wait;
   * deleting a live one costs a second install. Those are not the same size of
   * mistake.
   */
  isOwned() {
    if (this.released) return false;
    if (process.platform !== "win32") {
      let mine;
      let named;
      try {
        mine = fs.fstatSync(this.fd);
        named = fs.statSync(this.lockPath);
      } catch {
        return false;
      }
      if (mine.dev !== named.dev || mine.ino !== named.ino) return false;
    }
    const record = readLockRecord(this.lockPath);
    return record !== null && record.token === this.token;
  }

  /** @throws {LockError} kind `"lost"` */
  assertOwned() {
    if (!this.isOwned()) {
      throw new LockError("lost", `the install lock at ${this.lockPath} is no longer ours`, {
        lockPath: this.lockPath,
      });
    }
  }

  release() {
    if (this.released) return;
    if (this.timer !== null) clearInterval(this.timer);
    let mine = null;
    try {
      mine = fs.fstatSync(this.fd);
    } catch {
      mine = null;
    }
    const owned = this.isOwned();
    this.released = true;
    if (owned && mine !== null) {
      removeIfOurs(this.lockPath, mine, this.token);
    }
    try {
      fs.closeSync(this.fd);
    } catch {
      // Already closed.
    }
    // Dropped here rather than by the caller, so a guard released directly —
    // without `withInstallLock` — cannot leave this process believing it still
    // holds a lock it gave up.
    if (heldInThisProcess.get(this.processKey) === this) {
      heldInThisProcess.delete(this.processKey);
    }
    this.opts.onEvent("released", { lockPath: this.lockPath });
  }
}

function normalizeOptions(opts) {
  return {
    wait: opts.wait !== false,
    waitMs: opts.waitMs ?? DEFAULT_WAIT_MS,
    staleMs: opts.staleMs ?? DEFAULT_STALE_MS,
    pollMs: opts.pollMs ?? DEFAULT_POLL_MS,
    heartbeatMs: opts.heartbeatMs ?? DEFAULT_HEARTBEAT_MS,
    now: opts.now ?? Date.now,
    hostname: opts.hostname ?? os.hostname,
    probePid: opts.probePid ?? probePid,
    token: opts.token ?? crypto.randomUUID(),
    onEvent: opts.onEvent ?? (() => {}),
  };
}

/**
 * Whether an existing lock may be taken, and why. Null means it may not.
 *
 * @returns {string|null}
 */
function stealReason(lockPath, record, opts) {
  let stat;
  try {
    stat = fs.lstatSync(lockPath);
  } catch {
    // It went away between the failed create and this look: whoever held it has
    // released it, and retrying the create is the whole answer.
    return "released";
  }
  if (!stat.isFile()) {
    // A symlink, directory or fifo at this path is not a lock we wrote. It also
    // makes `open(path, "wx")` return EEXIST while `stat` says ENOENT, which is
    // an infinite loop for anything that trusts the second answer.
    return "not a regular file";
  }

  // Two clocks when the directory is remote. A negative age is clamped rather
  // than believed: "the server is ahead" must not mean "wedged forever".
  const ageMs = Math.max(0, opts.now() - stat.mtimeMs);

  // A pid is only meaningful on the machine that wrote it. On a shared
  // directory — or across a container's pid namespace — the number belongs to
  // somebody else's process table, so age is all there is.
  if (record && record.hostname === opts.hostname()) {
    if (opts.probePid(record.pid) === "dead") return `pid ${record.pid} is gone`;
  }

  if (ageMs > opts.staleMs) return `not refreshed for ${Math.round(ageMs)}ms`;
  return null;
}

/**
 * Take the install lock in `dir`, or report who has it.
 *
 * @param {string} dir must already exist
 * @param {object} [options]
 * @returns {Promise<LockGuard>}
 */
async function acquireInstallLock(dir, options = {}) {
  const opts = normalizeOptions(options);
  const lockPath = lockPathFor(dir);
  const key = processKey(dir);
  if (heldInThisProcess.has(key)) {
    throw new LockError("reentrant", `this process already holds ${lockPath}`, { lockPath });
  }

  const record = {
    pid: process.pid,
    hostname: opts.hostname(),
    token: opts.token,
    startedAt: opts.now(),
  };
  const deadline = opts.now() + opts.waitMs;
  let corroborating = null;

  for (;;) {
    let fd;
    try {
      fd = fs.openSync(lockPath, "wx", 0o600);
    } catch (err) {
      if (!err || err.code !== "EEXIST") {
        throw new LockError("io", `could not create ${lockPath}: ${err.message}`, { lockPath });
      }
      const held = readLockRecord(lockPath);
      const reason = stealReason(lockPath, held, opts);
      if (reason !== null) {
        // Corroborate before destroying anything: the same file, seen twice, at
        // least one poll apart. A successor that appeared while we were making
        // up our mind has a different inode, so this re-decides instead of
        // stealing from it. Only "released" skips it — there is nothing there.
        let identity = null;
        try {
          const s = fs.lstatSync(lockPath);
          identity = `${s.dev}:${s.ino}:${s.mtimeMs}`;
        } catch {
          identity = null;
        }
        if (identity !== null && corroborating !== identity) {
          corroborating = identity;
          await new Promise((resolve) => setTimeout(resolve, opts.pollMs));
          continue;
        }
        opts.onEvent("stale", { lockPath, reason, owner: held });
        if (identity !== null) {
          const [dev, ino] = identity.split(":");
          const judged = { dev: Number(dev), ino: Number(ino) };
          const outcome = removeIfOurs(lockPath, judged, opts.token);
          if (outcome === "removed") opts.onEvent("stole", { lockPath, owner: held });
        }
        corroborating = null;
        continue;
      }
      corroborating = null;
      if (!opts.wait || opts.now() >= deadline) {
        throw new LockError("held", `another install already holds ${lockPath}`, {
          lockPath,
          owner: held,
        });
      }
      opts.onEvent("waiting", { lockPath, owner: held });
      await new Promise((resolve) => setTimeout(resolve, opts.pollMs));
      continue;
    }

    try {
      fs.writeSync(fd, `${JSON.stringify(record)}\n`);
    } catch (err) {
      try {
        fs.closeSync(fd);
      } catch {
        // Nothing more to do with a descriptor we cannot write to.
      }
      throw new LockError("io", `could not write ${lockPath}: ${err.message}`, { lockPath });
    }

    const guard = new LockGuard(lockPath, fd, opts.token, opts, key);
    // A process mid-steal can remove what we just created. Without this check
    // we would run as a phantom holder, believing we had exclusivity we lost
    // microseconds ago.
    if (!guard.isOwned()) {
      try {
        fs.closeSync(fd);
      } catch {
        // Nothing to do.
      }
      opts.onEvent("lost", { lockPath });
      continue;
    }

    sweepLockLitter(dir, opts.staleMs, opts.now());
    guard.startHeartbeat();
    heldInThisProcess.set(key, guard);
    opts.onEvent("acquired", { lockPath, token: opts.token });
    return guard;
  }
}

// A Ctrl-C in a `postinstall` is by far the likeliest way a stale lock is
// created. Every syscall in `release` is synchronous, so an exit handler can
// actually finish the job.
let signalHandlersInstalled = false;
function installExitHandlers() {
  if (signalHandlersInstalled) return;
  signalHandlersInstalled = true;
  const releaseAll = () => {
    for (const guard of heldInThisProcess.values()) {
      try {
        guard.release();
      } catch {
        // Exiting anyway.
      }
    }
    heldInThisProcess.clear();
  };
  process.on("exit", releaseAll);
  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.on(signal, () => {
      releaseAll();
      process.removeAllListeners(signal);
      process.kill(process.pid, signal);
    });
  }
}

/**
 * Run `fn` holding the install lock in `dir`, releasing it however `fn` ends.
 *
 * `fn` receives the guard, so a long job can `heartbeat()` across a synchronous
 * stretch and `assertOwned()` before any step that must not happen after
 * dispossession.
 *
 * @template T
 * @param {string} dir
 * @param {(guard: LockGuard) => Promise<T>|T} fn
 * @param {object} [options]
 * @returns {Promise<T>}
 */
async function withInstallLock(dir, fn, options = {}) {
  installExitHandlers();
  const guard = await acquireInstallLock(dir, options);
  try {
    return await fn(guard);
  } finally {
    guard.release();
  }
}

module.exports = {
  LOCK_FILE,
  DEFAULT_STALE_MS,
  DEFAULT_HEARTBEAT_MS,
  DEFAULT_WAIT_MS,
  LockError,
  LockGuard,
  lockPathFor,
  readLockRecord,
  probePid,
  acquireInstallLock,
  withInstallLock,
};
