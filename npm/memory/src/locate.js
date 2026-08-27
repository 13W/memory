"use strict";

// Finding the native binaries this package installed — the read side of
// `install.js`.
//
// RESOLUTION PICKS A DIRECTORY, NOT A FILE, and that is a requirement rather
// than a convenience. Spec 13 §4 `[SPEC, ADR-0013]` says the daemon MUST sit
// beside the proxy that spawns it, because "the version comes from whichever
// binary is found next to this proxy" is what makes the upgrade trigger a
// definition instead of a coincidence — `local-rag-proxy` finds its daemon by
// looking in its own directory (`crates/local-rag-proxy/src/connect.rs:55`),
// and the ONNX runtime is found the same way. A resolver that answered each
// binary separately could hand back a proxy from one rung and a daemon from
// another, and nothing downstream would notice until the versions disagreed.
// So a rung counts only when it holds *every required binary*, and callers get
// the directory that won.
//
// THIS IS NOT THE ORDER IN SPEC 13 §2, and the difference is deliberate. That
// list — override, then `PATH`, then well-known global-bin directories — is the
// *plugin's*, and its amendment note says so: "`T22-12`/`T22-13` implement it".
// The plugin resolves an executable it did not install and has nothing but
// `PATH` to go on. This module resolves what this package put down itself, so
// it knows where to look, and it has one obligation the plugin does not: a
// developer's own build must win over anything downloaded.
//
// THE RUNGS, and what each one refuses to do.
//
//   1. `LOCAL_RAG_BIN_DIR` — final. If the directory does not hold the
//      binaries, that is an error, not a reason to look elsewhere. ADR-0013
//      introduced it as the answer to air-gapped installation, "which wins
//      over everything and never downloads"; an override that is silently
//      ignored is worse than one that does not exist.
//   2. A source checkout — `target/release`, then `target/debug`. Also final:
//      being in a checkout with nothing built means "build it", not "quietly
//      run a download from three weeks ago that does not match this source".
//   3. `<pkg>/bin`, and 4. the per-user cache — both only behind a current
//      install manifest.
//
// Checkout ranks above `<pkg>/bin` on purpose. In a checkout `<pkg>/bin` holds
// the committed shims, and a stray `npm install` inside it can drop downloaded
// binaries there — which would shadow the developer's own build with a release
// build of something else. The order is the defence.
//
// A manifest is required on rungs 3 and 4 and not on 1 and 2, because the
// manifest certifies what *the installer* put down. An override is the user's
// word and a checkout is the user's build; neither was installed, so neither
// has a manifest to show, and demanding one would make both unusable.
//
// This module prints nothing. On the MCP path stdout is the JSON-RPC stream
// (13 §2: "stdout stays byte-empty"), so where a diagnostic goes is the calling
// shim's decision, never a library's.

const fs = require("node:fs");
const path = require("node:path");

const { platformKey, targetTriple } = require("./platform");
const { PRODUCT_BINARIES, executableName } = require("./release");
const { installDir, PathsError } = require("./paths");
const { readManifest, manifestIsCurrent, ERROR_FILE } = require("./install");
const {
  formatNotInstalledError,
  formatSourceCheckoutNotBuiltError,
  formatOverrideMissingError,
  formatAssetAbsentError,
} = require("./errors");

const BIN_DIR_VAR = "LOCAL_RAG_BIN_DIR";

// A candidate repository root must have both. Either alone is common enough to
// be met by accident; together they are this workspace and the release config
// that produces the very binaries being looked for. Deliberately not `.git`: a
// `git archive` export or a vendored copy is still a checkout somebody can
// build from, and a `pnpm link`ed copy sitting inside an unrelated git
// repository would match it for the wrong reason.
const CHECKOUT_MARKERS = Object.freeze(["Cargo.toml", "dist-workspace.toml"]);

/** An empty variable is unset, the same rule `paths.js` follows. */
function nonemptyVar(env, name) {
  const value = env[name];
  return typeof value === "string" && value !== "" ? value : null;
}

/**
 * A regular file we could actually exec.
 *
 * The execute bit is checked on POSIX because a file of the right name that
 * cannot be run is not a binary — reporting it as found only moves the failure
 * to exec time, where it arrives as a shell error instead of this module's
 * message. Windows has no such bit and decides by extension.
 */
function isExecutableFile(file) {
  let stat;
  try {
    stat = fs.statSync(file);
  } catch {
    return false;
  }
  if (!stat.isFile()) return false;
  if (process.platform === "win32") return true;
  return (stat.mode & 0o111) !== 0;
}

/**
 * Whether `dir` holds every required binary.
 *
 * @returns {{ok: true} | {ok: false, missing: string, why: string}}
 */
function directoryHolds(dir, opts) {
  for (const binary of opts.binaries) {
    if (!binary.required) continue;
    const file = path.join(dir, executableName(binary.name, opts.platform));
    if (!isExecutableFile(file)) {
      return { ok: false, missing: binary.name, why: `no executable ${path.basename(file)}` };
    }
  }
  return { ok: true };
}

/**
 * The repository root this package is checked out in, or null.
 *
 * Anchored on `packageDir` — which callers derive from `__dirname`, never from
 * `process.argv[1]`: `argv[1]` is not resolved through a symlink even in Node's
 * default mode, and for a global bin shim it is exactly a symlink path.
 *
 * `realpathSync` on both sides is what makes this correct under
 * `--preserve-symlinks` as well as without it. By default `__dirname` is
 * already the real path, so a `pnpm link --global` package sees the checkout
 * with no help; under that flag it stays at the link, the walk lands somewhere
 * in a package store, and the identity check below is what makes us fall
 * through honestly instead of adopting a wrong root.
 *
 * @param {string} packageDir the package's own root (`.../npm/memory`)
 * @returns {string|null}
 */
function sourceCheckoutRoot(packageDir) {
  let self;
  try {
    self = fs.realpathSync(packageDir);
  } catch {
    return null;
  }
  // `npm/memory` → `npm` → the root. There is nothing to search for.
  const root = path.resolve(self, "..", "..");
  for (const marker of CHECKOUT_MARKERS) {
    if (!fs.existsSync(path.join(root, marker))) return null;
  }
  // And the tree's own `npm/memory` must be *this* package. Without it, a
  // package copied into somebody else's checkout at the same depth would claim
  // that checkout's `target/`.
  let nested;
  try {
    nested = fs.realpathSync(path.join(root, "npm", "memory"));
  } catch {
    return null;
  }
  return nested === self ? root : null;
}

function normalizeOptions(options) {
  const platform = options.platform ?? process.platform;
  const arch = options.arch ?? process.arch;
  return {
    env: options.env ?? process.env,
    platform,
    arch,
    key: options.key ?? platformKey(platform, arch),
    packageDir: options.packageDir ?? path.resolve(__dirname, ".."),
    packageVersion: options.packageVersion ?? require("../package.json").version,
    binaries: options.binaries ?? PRODUCT_BINARIES,
    pathsOpts: { platform, homedir: options.homedir },
  };
}

function found(source, dir, candidates, extra = {}) {
  return { ok: true, source, dir, candidates, ...extra };
}

function failed(reason, message, candidates) {
  return { ok: false, reason, message, candidates };
}

/**
 * The directory every product binary should be taken from.
 *
 * @param {object} [options]
 * @returns {{ok: true, source: "override"|"checkout"|"package"|"cache", dir: string,
 *   candidates: object[], repoRoot?: string, tag?: string}
 *   | {ok: false, reason: "override-missing"|"checkout-not-built"|"not-installed"
 *      |"unsupported-platform", message: string, candidates: object[]}}
 */
function locateBinDir(options = {}) {
  const opts = normalizeOptions(options);
  const candidates = [];

  if (targetTriple(opts.key) === null) {
    return failed(
      "unsupported-platform",
      formatNotInstalledError({ key: opts.key }),
      candidates,
    );
  }

  const override = nonemptyVar(opts.env, BIN_DIR_VAR);
  if (override !== null) {
    const check = directoryHolds(override, opts);
    candidates.push({ source: "override", dir: override, why: check.ok ? null : check.why });
    if (check.ok) return found("override", override, candidates);
    // Final by design: an explicit override is never silently ignored.
    return failed(
      "override-missing",
      formatOverrideMissingError({
        dir: override,
        binary: executableName(check.missing, opts.platform),
        envVar: BIN_DIR_VAR,
      }),
      candidates,
    );
  }

  const repoRoot = sourceCheckoutRoot(opts.packageDir);
  if (repoRoot !== null) {
    let missing = opts.binaries.find((b) => b.required).name;
    for (const profile of ["release", "debug"]) {
      const dir = path.join(repoRoot, "target", profile);
      const check = directoryHolds(dir, opts);
      candidates.push({ source: "checkout", dir, why: check.ok ? null : check.why });
      if (check.ok) return found("checkout", dir, candidates, { repoRoot });
      if (profile === "release") missing = check.missing;
    }
    // Also final. Falling through to a download in a checkout would run
    // something other than the source that is sitting right there.
    return failed(
      "checkout-not-built",
      formatSourceCheckoutNotBuiltError({
        repoRoot,
        binary: missing,
      }),
      candidates,
    );
  }

  const installed = [{ source: "package", dir: path.join(opts.packageDir, "bin") }];
  try {
    installed.push({
      source: "cache",
      dir: installDir(opts.key, opts.env, opts.pathsOpts),
    });
  } catch (err) {
    if (!(err instanceof PathsError)) throw err;
    candidates.push({ source: "cache", dir: null, why: err.message });
  }

  for (const { source, dir } of installed) {
    const manifest = readManifest(dir);
    if (!manifestIsCurrent(manifest, opts, dir)) {
      candidates.push({ source, dir, why: "no current install manifest" });
      continue;
    }
    const check = directoryHolds(dir, opts);
    candidates.push({ source, dir, why: check.ok ? null : check.why });
    if (check.ok) return found(source, dir, candidates, { tag: manifest.tag });
  }

  return failed("not-installed", formatNotInstalledError({ key: opts.key }), candidates);
}

/**
 * One binary, taken from whichever directory `locateBinDir` chose.
 *
 * @param {string} name e.g. "local-rag-proxy"
 * @param {object} [options]
 * @returns {object} `locateBinDir`'s result plus `path`, or a failure whose
 *   `reason` is `"binary-absent"` when the directory resolved but this
 *   particular (optional) binary is not in it.
 */
function locateBinary(name, options = {}) {
  const located = locateBinDir(options);
  if (!located.ok) return located;
  const opts = normalizeOptions(options);
  const file = path.join(located.dir, executableName(name, opts.platform));
  if (!isExecutableFile(file)) {
    // Only an optional binary can reach here — the required set was checked
    // when the directory was chosen — so this is "the release did not carry
    // it", not "nothing is installed". Saying the latter would send a user to
    // reinstall something that is already there and would not help.
    const tag = located.tag ?? null;
    return failed(
      "binary-absent",
      tag === null
        ? formatNotInstalledError({ key: opts.key })
        : formatAssetAbsentError({ binary: name, tag, key: opts.key }),
      located.candidates,
    );
  }
  return { ...located, path: file };
}

/**
 * Everything a diagnostic surface needs — `doctor` (T22-16) reads this.
 *
 * `candidates` is the ordered list of rungs actually consulted, each with the
 * reason it was passed over. That is the difference between telling a user
 * "not installed" and telling them why: the manifest was for another version,
 * or the cache directory was never written, or `LOCAL_RAG_BIN_DIR` points
 * somewhere empty.
 */
function installInfo(options = {}) {
  const opts = normalizeOptions(options);
  const located = locateBinDir(options);
  const dir = located.ok ? located.dir : null;
  const manifest = dir === null ? null : readManifest(dir);

  const binaries = {};
  for (const binary of opts.binaries) {
    const file =
      dir === null ? null : path.join(dir, executableName(binary.name, opts.platform));
    binaries[binary.name] = {
      required: binary.required,
      path: file !== null && isExecutableFile(file) ? file : null,
    };
  }

  let error = null;
  try {
    const cache = installDir(opts.key, opts.env, opts.pathsOpts);
    error = fs.readFileSync(path.join(cache, ERROR_FILE), "utf8").trim();
  } catch {
    error = null;
  }

  return {
    packageVersion: opts.packageVersion,
    key: opts.key,
    triple: targetTriple(opts.key),
    source: located.ok ? located.source : null,
    dir,
    tag: manifest === null ? null : manifest.tag,
    reason: located.ok ? null : located.reason,
    message: located.ok ? null : located.message,
    binaries,
    manifest,
    error,
    candidates: located.candidates,
  };
}

module.exports = {
  BIN_DIR_VAR,
  CHECKOUT_MARKERS,
  isExecutableFile,
  sourceCheckoutRoot,
  locateBinDir,
  locateBinary,
  installInfo,
};
