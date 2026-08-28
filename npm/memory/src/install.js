"use strict";

// Putting the native binaries on disk — the module that decides what to trust
// and where it goes.
//
// The pieces below it each refuse to make that decision. `http.js` streams an
// asset and reports the sha256 it computed, without ever comparing it to
// anything. `archive.js` takes one member out of an archive and writes it
// where it is told, without renaming, chmod-ing or cleaning up. `lock.js`
// keeps two installers from doing the same work twice, and says in its own
// header that it is not what makes any of this correct. This module is where
// those become an installation.
//
// THE ORDERING IS `crates/models/src/install.rs`'s, and spec 10 §5's
// `[FIXED policy]` states it: per file, `.part` → stream while hashing →
// fsync → verify the digest → `rename` → fsync the *directory* → set the mode.
// The directory fsync is not decoration: a rename that survives a crash while
// its directory entry does not leaves a file that appears absent on the next
// boot, which is the exact failure the marker ordering exists to exclude. And
// the mode is set after the rename, on the final path, as it is there.
//
// AN INSTALL IS A SET, NOT A SEQUENCE OF FILES. Spec 13 §4 `[SPEC, ADR-0013]`
// requires every product binary to sit in one flat directory, because
// `local-rag-proxy` finds its daemon by looking beside itself
// (`crates/local-rag-proxy/src/connect.rs:55`) and the ONNX runtime is found
// the same way. Two consequences follow, and both are visible below: the tag
// is resolved once for the whole install rather than per asset — otherwise a
// release cut halfway through yields `local-rag` from one tag and
// `local-rag-proxy` from another — and the manifest is written last, so a set
// that is not complete is indistinguishable from no set at all.
//
// WHERE THIS DIVERGES FROM `install.rs`, each with its reason.
//
// The `.part` files live in a subdirectory of the destination rather than
// beside their targets. There, every intermediate file is a future product
// file; here one of them is a twelve-megabyte archive that will never be one,
// and the destination has to stay a flat directory of executables. The
// subdirectory is inside the destination, so it is the same filesystem and
// `rename` is still atomic. It is also per attempt, named after the lock's
// token, which is what makes two simultaneous installs harmless rather than
// mutually destructive.
//
// Nothing is ever reused. `install.rs` keeps a file already on disk whose
// digest matches the catalog's pinned one; there is no pinned digest here, and
// the sidecar certifies the *archive*, not the executable inside it. So an
// executable already in place tells us nothing until the manifest exists, and
// the manifest is written last. A resumed install refetches everything. That
// is the price of `latest`, paid in bandwidth on a rare path rather than in
// trust.
//
// A `.part` whose digest did not match is deleted, where `install.rs` leaves
// it for inspection. Its `.part` sits in the user's own model directory and is
// overwritten by the retry; ours sits in scratch space that is about to be
// swept, and a corrupt archive left there is litter, not evidence.
//
// One manifest file instead of `manifest.json` plus an `.ok` marker. Theirs are
// separate because the manifest is a record in its own right — licence,
// source, revision — and `.ok` is the readiness bit. Ours is small and written
// atomically, so it appears whole; there is nothing to separate.
//
// AND OURS IS READ, WHERE THEIRS NEVER IS. That difference needs stating
// plainly, because ADR-0005 §6's standard — "verification uses the compiled-in
// catalog, so a tampered manifest cannot talk the installer into accepting
// different bytes" — is narrowed by ADR-0013 but not abandoned. What a
// tampered manifest here cannot do is make this module accept different bytes:
// every byte is checked against a sidecar fetched in the same run, and the
// manifest is consulted only for `packageVersion` and the tag. The most it can
// buy is a skipped install — and anyone who can write in the install directory
// can replace the binaries outright, so it grants no power that was not
// already there. It is therefore read as untrusted input: defensively parsed,
// and a manifest that is malformed, foreign or stale means "not installed"
// rather than an exception.
//
// THE MODE IS 0755, NOT 0600. The difference that matters is the execute bit,
// which 0600 withholds even from the owner. The confidentiality 0600 buys for
// model weights is provided here by the containing directory, which is 0700
// under the store (spec 02 §2.1). And when the destination is a global
// `<pkg>/bin` instead, that directory is world-readable by design, and 0755 is
// what makes a machine-wide `npm install --global` usable by anybody but the
// user who ran it.
//
// SIZE IS NOT VERIFIED, because the sidecar does not carry one — `install.rs`
// has a `SizeMismatch` distinct from `ChecksumMismatch` and we cannot. The
// partial analogue that does exist is `http.js`'s cross-check of
// `content-length` when the server sends it.

const fs = require("node:fs");
const path = require("node:path");

const { platformKey, targetTriple, isSupported } = require("./platform");
const {
  PRODUCT_BINARIES,
  assetName,
  sidecarName,
  executableName,
  pinnedAssetUrl,
  parseSha256Sidecar,
} = require("./release");
const { HttpError, resolveLatestTag, httpGetToString, httpGetToFile } = require("./http");
const { extractSingleMember } = require("./archive");
const { withInstallLock, DEFAULT_STALE_MS } = require("./lock");
const {
  formatChecksumMismatchError,
  formatAssetAbsentError,
  formatDownloadError,
} = require("./errors");

const MANIFEST_FILE = ".local-rag-install.json";
const ERROR_FILE = ".local-rag-install.error";
const TMP_PREFIX = ".local-rag-install.tmp-";
const PART_SUFFIX = ".part";

// Bumping this invalidates every manifest written by an older shape, the same
// way `projection_schema_version` invalidates a persisted projection.
const MANIFEST_VERSION = 1;

const EXECUTABLE_MODE = 0o755;

/** Errors carry a kind so a caller can tell a bad download from a bad disk. */
class InstallError extends Error {
  /** @param {"checksum"|"asset-absent"|"download"|"io"|"unsupported-platform"} kind */
  constructor(kind, message, { binary, asset } = {}) {
    super(message);
    this.name = "InstallError";
    this.kind = kind;
    this.binary = binary;
    this.asset = asset;
  }
}

/** @param {unknown} err @returns {boolean} */
function isNotFound(err) {
  return err instanceof HttpError && err.kind === "status" && err.status === 404;
}

/**
 * `<name>.part`, by appending rather than by replacing an extension.
 *
 * `install.rs` has a test pinning exactly this (`part_path`): `with_extension`
 * would turn `model_quantized.onnx` into `model_quantized.part` and collide
 * with a sibling. The JS equivalent of that mistake is anything built on
 * `path.extname`.
 */
function partPath(target) {
  return `${target}${PART_SUFFIX}`;
}

/**
 * fsync a directory so a rename into it is durable.
 *
 * Skipped where the platform has no such notion: a directory cannot be opened
 * as a file on Windows, where the rename is still atomic. `EPERM`/`EACCES` are
 * swallowed as `install.rs` swallows `PermissionDenied`, and `EINVAL` with it —
 * a filesystem answering "no such concept" is the same answer as Windows.
 */
function syncDir(dir) {
  if (process.platform === "win32") return;
  let fd;
  try {
    fd = fs.openSync(dir, "r");
  } catch (err) {
    if (err && (err.code === "EPERM" || err.code === "EACCES" || err.code === "EINVAL")) return;
    throw err;
  }
  try {
    fs.fsyncSync(fd);
  } catch (err) {
    if (!err || (err.code !== "EPERM" && err.code !== "EACCES" && err.code !== "EINVAL")) {
      throw err;
    }
  } finally {
    fs.closeSync(fd);
  }
}

/** fsync a file that is already fully written. */
function syncFile(file) {
  const fd = fs.openSync(file, "r+");
  try {
    fs.fsyncSync(fd);
  } finally {
    fs.closeSync(fd);
  }
}

/**
 * The last three beats, shared by every file this module puts down: make the
 * bytes durable, move them into place, make the move durable, then set the
 * mode on the final path.
 */
function renameIntoPlace(from, to, mode) {
  syncFile(from);
  fs.renameSync(from, to);
  syncDir(path.dirname(to));
  if (process.platform !== "win32") {
    fs.chmodSync(to, mode);
  }
}

/** `install.rs`'s `atomic_write`, minus the digest it has no use for here. */
function atomicWriteFile(target, contents, mode) {
  const part = partPath(target);
  fs.writeFileSync(part, contents);
  renameIntoPlace(part, target, mode);
}

/** @param {string} dir @returns {string} */
function manifestPathFor(dir) {
  return path.join(dir, MANIFEST_FILE);
}

/**
 * The install manifest, or null when there is nothing readable to return.
 *
 * Never throws: see the header on why this file is untrusted input.
 *
 * @param {string} dir @returns {object|null}
 */
function readManifest(dir) {
  let text;
  try {
    text = fs.readFileSync(manifestPathFor(dir), "utf8");
  } catch {
    return null;
  }
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    return null;
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) return null;
  return parsed;
}

/**
 * Whether `manifest` describes an installation this package can use right now.
 *
 * The file-existence check is `existsSync`, not a digest: re-hashing a hundred
 * megabytes to answer "is it installed" would make every resolution cost more
 * than the thing it is resolving. Detecting a *modified* binary is not this
 * check's job — nothing here could distinguish that from a deliberate local
 * build anyway.
 *
 * @param {object|null} manifest
 * @param {{packageVersion: string, key: string, platform?: string}} expected
 * @param {string} dir
 * @returns {boolean}
 */
function manifestIsCurrent(manifest, expected, dir) {
  if (manifest === null) return false;
  if (manifest.manifestVersion !== MANIFEST_VERSION) return false;
  if (manifest.packageVersion !== expected.packageVersion) return false;
  if (manifest.platformKey !== expected.key) return false;
  if (typeof manifest.tag !== "string" || manifest.tag === "") return false;
  const binaries = manifest.binaries;
  if (binaries === null || typeof binaries !== "object") return false;
  for (const entry of Object.values(binaries)) {
    if (entry === null || typeof entry !== "object") return false;
    if (entry.state !== "installed") continue;
    if (typeof entry.file !== "string" || !fs.existsSync(path.join(dir, entry.file))) {
      return false;
    }
  }
  return true;
}

/** Remove per-attempt scratch directories left by runs that never finished. */
function sweepStaleTempDirs(dir, staleMs, now) {
  let entries;
  try {
    entries = fs.readdirSync(dir);
  } catch {
    return;
  }
  for (const name of entries) {
    if (!name.startsWith(TMP_PREFIX)) continue;
    const full = path.join(dir, name);
    try {
      // Only by age, and only while holding the lock: a concurrent attempt's
      // scratch directory is fresh, and sweeping it would break a download
      // that is going perfectly well.
      if (now - fs.statSync(full).mtimeMs > staleMs) {
        fs.rmSync(full, { recursive: true, force: true });
      }
    } catch {
      // Gone already, or not ours to remove.
    }
  }
}

/**
 * Fetch, verify and place one binary.
 *
 * The sidecar is fetched first and its presence is what defines the asset's:
 * without it the archive cannot be verified, and ADR-0013 forbids installing
 * what has not been. An archive present without its sidecar is a broken
 * release, and stays loud.
 */
async function installOne(binary, ctx) {
  const asset = assetName(binary.name, ctx.key);
  const exeName = executableName(binary.name, ctx.platform);

  let expected;
  try {
    const sidecarUrl = pinnedAssetUrl(ctx.tag, sidecarName(asset), ctx.env);
    expected = parseSha256Sidecar(await httpGetToString(sidecarUrl, { env: ctx.env }), asset);
  } catch (err) {
    if (isNotFound(err)) {
      if (!binary.required) {
        ctx.log(`skipping ${binary.name}: release ${ctx.tag} does not carry it`);
        return { state: "absent" };
      }
      throw new InstallError(
        "asset-absent",
        // `othersInstalled: false` — this path is required-only, and the
        // failure handler removes the whole scratch directory, so the default
        // wording ("the other binaries installed normally") would be false in
        // the one situation it is ever printed.
        formatAssetAbsentError({
          binary: binary.name,
          tag: ctx.tag,
          key: ctx.key,
          othersInstalled: false,
        }),
        { binary: binary.name, asset },
      );
    }
    throw new InstallError("download", describeFetchFailure(err, ctx, sidecarName(asset)), {
      binary: binary.name,
      asset,
    });
  }

  const archivePath = path.join(ctx.tmpDir, asset);
  const archiveUrl = pinnedAssetUrl(ctx.tag, asset, ctx.env);
  let sha256;
  try {
    ({ sha256 } = await httpGetToFile(archiveUrl, archivePath, { env: ctx.env }));
  } catch (err) {
    throw new InstallError("download", describeFetchFailure(err, ctx, asset), {
      binary: binary.name,
      asset,
    });
  }

  if (sha256 !== expected) {
    // No cleanup here: every throw out of this function unwinds into the caller
    // below, which removes the whole scratch directory. A second, narrower
    // delete beside this one looked like care and was measurably dead — a
    // mutation that removed it changed no test, because the outer sweep is what
    // makes "no `.part` survives a mismatch" true.
    throw new InstallError(
      "checksum",
      formatChecksumMismatchError({ asset, expected, actual: sha256 }),
      { binary: binary.name, asset },
    );
  }

  const stagedExe = path.join(ctx.tmpDir, partPath(exeName));
  extractSingleMember(archivePath, exeName, stagedExe);
  fs.rmSync(archivePath, { force: true });
  renameIntoPlace(stagedExe, path.join(ctx.destDir, exeName), EXECUTABLE_MODE);

  ctx.log(`installed ${exeName}`);
  return { state: "installed", file: exeName, archiveSha256: sha256 };
}

function describeFetchFailure(err, ctx, asset) {
  return formatDownloadError({
    url: pinnedAssetUrl(ctx.tag, asset, ctx.env),
    cause: err && err.message ? err.message : String(err),
  });
}

function normalizeOptions(destDir, options) {
  const platform = options.platform ?? process.platform;
  const arch = options.arch ?? process.arch;
  const key = options.key ?? platformKey(platform, arch);
  return {
    env: options.env ?? process.env,
    platform,
    key,
    packageVersion: options.packageVersion ?? require("../package.json").version,
    binaries: options.binaries ?? PRODUCT_BINARIES,
    mode: options.mode ?? "update",
    dirMode: options.dirMode,
    log: options.log ?? (() => {}),
    lock: options.lock ?? {},
    now: options.now ?? Date.now,
    staleMs: options.staleMs ?? DEFAULT_STALE_MS,
    destDir,
  };
}

function skipReport(reason, manifest) {
  return {
    skipped: true,
    reason,
    tag: manifest === null ? null : manifest.tag,
    installed: [],
    absent: [],
  };
}

/**
 * Install every product binary for this platform into `destDir`.
 *
 * @param {string} destDir created if missing
 * @param {object} [options]
 * @param {"if-needed"|"update"|"force"} [options.mode] `if-needed` touches the
 *   network only when the manifest says nothing is there — the path a shim
 *   takes on a cold start, which cannot afford a round trip. `update`
 *   re-resolves `latest` and reinstalls when the tag moved, which is what makes
 *   "update the package, get the new services" true (ADR-0013 Decision 2).
 *   `force` reinstalls regardless.
 * @returns {Promise<{skipped: boolean, reason?: string, tag: string|null,
 *   installed: string[], absent: string[]}>}
 */
async function installBinaries(destDir, options = {}) {
  const opts = normalizeOptions(destDir, options);

  if (!isSupported(opts.key) || targetTriple(opts.key) === null) {
    throw new InstallError(
      "unsupported-platform",
      `local-rag: no release binaries are published for ${opts.key}`,
    );
  }

  // The cheap short-circuit, before the lock and before the network — the same
  // place `install.rs` puts its `.ok` check.
  if (opts.mode === "if-needed") {
    const manifest = readManifest(destDir);
    if (manifestIsCurrent(manifest, opts, destDir)) {
      return skipReport("already-installed", manifest);
    }
  }

  fs.mkdirSync(destDir, { recursive: true });
  if (opts.dirMode !== undefined && process.platform !== "win32") {
    // Only when the caller knows which policy applies: this module cannot tell
    // a private store directory from a world-readable global bin, and must not
    // impose either.
    fs.chmodSync(destDir, opts.dirMode);
  }

  return withInstallLock(
    destDir,
    async (guard) => {
      const manifest = readManifest(destDir);
      if (opts.mode === "if-needed" && manifestIsCurrent(manifest, opts, destDir)) {
        return skipReport("already-installed", manifest);
      }

      sweepStaleTempDirs(destDir, opts.staleMs, opts.now());
      fs.rmSync(path.join(destDir, ERROR_FILE), { force: true });

      const first = opts.binaries.find((b) => b.required) ?? opts.binaries[0];
      let tag;
      try {
        tag = await resolveLatestTag(assetName(first.name, opts.key), { env: opts.env });
      } catch (err) {
        throw new InstallError(
          "download",
          formatDownloadError({
            url: `${opts.env.LOCAL_RAG_RELEASE_BASE_URL ?? "the release channel"}/latest`,
            cause: err && err.message ? err.message : String(err),
          }),
        );
      }

      if (
        opts.mode !== "force" &&
        manifestIsCurrent(manifest, opts, destDir) &&
        manifest.tag === tag
      ) {
        return skipReport("up-to-date", manifest);
      }

      const tmpDir = path.join(destDir, TMP_PREFIX + guard.token);
      fs.mkdirSync(tmpDir, { recursive: true });
      const ctx = { ...opts, destDir, tmpDir, tag };

      const binaries = {};
      const installed = [];
      const absent = [];
      try {
        for (const binary of opts.binaries) {
          const result = await installOne(binary, ctx);
          binaries[binary.name] = result;
          (result.state === "installed" ? installed : absent).push(binary.name);
          // An explicit pulse beside the timer's: `archive.js` is synchronous
          // and a long enough stretch of it would keep the interval from
          // firing at all.
          guard.heartbeat();
        }
      } catch (err) {
        // The whole scratch directory, on every failure path: `install.rs`
        // leaves a mismatched `.part` for inspection because its own sits in
        // the user's model directory and a retry overwrites it, while ours is
        // scratch space that is about to be swept anyway. This one line is what
        // makes "a failed install leaves no partial download" true.
        fs.rmSync(tmpDir, { recursive: true, force: true });
        try {
          fs.writeFileSync(path.join(destDir, ERROR_FILE), `${err.message}\n`);
        } catch {
          // A directory we cannot write to is already the bigger problem.
        }
        throw err;
      }

      fs.rmSync(tmpDir, { recursive: true, force: true });

      // Nothing durable has been published yet. If we were dispossessed while
      // downloading, another installer may have been filling this directory
      // from a different tag, and a manifest naming ours would describe a set
      // that does not exist. See `lock.js`'s header.
      guard.assertOwned();

      const contents = {
        manifestVersion: MANIFEST_VERSION,
        packageVersion: opts.packageVersion,
        platformKey: opts.key,
        targetTriple: targetTriple(opts.key),
        tag,
        binaries,
      };
      atomicWriteFile(manifestPathFor(destDir), `${JSON.stringify(contents, null, 2)}\n`, 0o644);
      opts.log(`installed ${installed.length} binaries from ${tag}`);
      return { skipped: false, tag, installed, absent };
    },
    opts.lock,
  );
}

module.exports = {
  MANIFEST_FILE,
  MANIFEST_VERSION,
  ERROR_FILE,
  TMP_PREFIX,
  EXECUTABLE_MODE,
  InstallError,
  manifestPathFor,
  readManifest,
  manifestIsCurrent,
  installBinaries,
};
