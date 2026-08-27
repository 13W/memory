"use strict";

// Where things live on disk — a mirror of `crates/core/src/paths/mod.rs`.
//
// This module exists because two independent implementations now have to agree
// about one directory. The daemon computes its store root in Rust; this package
// installs binaries into a subtree of it and later looks for them there. If the
// two formulas drift, nothing errors — the installer simply writes where the
// daemon will not look, and the failure surfaces much later as "not installed"
// on a machine where everything was installed.
//
// So the rules below are not a reasonable-looking approximation of a data
// directory. They are `data_dir` from `crates/core/src/paths/mod.rs:180-221`,
// normative in spec 02 §2.1, transcribed with each of its four traps kept:
//
//   1. `LOCAL_RAG_HOME` IS the data directory, not its parent. The store root
//      is `<data_dir>/local-rag`, added by `StoreLayout::resolve`.
//   2. An empty variable counts as unset (XDG). `env.X || fallback` gives that;
//      `env.X ?? fallback` does not, and would resolve to "".
//   3. `XDG_DATA_HOME` is ignored unless absolute — but `LOCAL_RAG_HOME` is
//      NOT put through that test and is taken as given, relative or not. The
//      asymmetry is real and deliberate on the Rust side; copying only the
//      symmetric-looking half would be a divergence.
//   4. macOS takes the POSIX branch. There is no `~/Library/Application
//      Support` anywhere in this project, and spec 02 §2.1 says so in as many
//      words: "macOS is a POSIX target and uses the XDG fallbacks".
//
// On Windows the home directory never participates — only `LOCALAPPDATA` — so
// the one place `os.homedir()` and Rust's `std::env::home_dir()` could disagree
// is unreachable. On POSIX they agree: `$HOME` first, the passwd database
// after.
//
// No socket, no network, no writes: this module only computes strings.

const path = require("node:path");
const os = require("node:os");

const { targetTriple } = require("./platform");

/**
 * The path flavour for a platform, chosen explicitly rather than taken from the
 * ambient `path` module.
 *
 * D-055's lesson, in this repository, in this language: `npmGlobalNodeModules`
 * had to use `path.win32`/`path.posix` by name because a cross-platform formula
 * test run on a POSIX host against the win32 branch otherwise joins with `/`
 * and passes for the wrong reason — or fails for one. Every function here takes
 * a `platform`, so every one of them has to do this.
 */
function flavour(platform) {
  return platform === "win32" ? path.win32 : path.posix;
}

/** Errors carry a kind so a caller can tell "unset" from "malformed". */
class PathsError extends Error {
  /** @param {"no-base-dir"|"unsupported-platform"} kind */
  constructor(kind, message) {
    super(message);
    this.name = "PathsError";
    this.kind = kind;
  }
}

/** An empty value is unset, per the XDG base directory spec. */
function nonemptyVar(env, name) {
  const value = env[name];
  return typeof value === "string" && value !== "" ? value : null;
}

/**
 * `<data_dir>` (spec 02 §2.1).
 *
 * POSIX: `$LOCAL_RAG_HOME`, else `$XDG_DATA_HOME` when absolute, else
 * `<home>/.local/share`. Windows: `$LOCAL_RAG_HOME`, else `%LOCALAPPDATA%`.
 *
 * @param {NodeJS.ProcessEnv} [env]
 * @param {{platform?: string, homedir?: () => string}} [opts] injected so the
 *   other platform's branch is testable from this one without touching global
 *   state — the same seam `platform.js` takes a `platform` argument for.
 * @returns {string}
 */
function dataDir(env = process.env, opts = {}) {
  const platform = opts.platform ?? process.platform;
  const home = opts.homedir ?? os.homedir;

  const explicit = nonemptyVar(env, "LOCAL_RAG_HOME");
  if (explicit !== null) {
    // Deliberately not run through the absolute-path test above: Rust does not
    // either, and a relative `LOCAL_RAG_HOME` is a supported container idiom.
    return explicit;
  }

  if (platform === "win32") {
    const local = nonemptyVar(env, "LOCALAPPDATA");
    if (local !== null) {
      return local;
    }
    throw new PathsError(
      "no-base-dir",
      "cannot resolve data_dir: set LOCAL_RAG_HOME or the platform data/config directory",
    );
  }

  const xdg = nonemptyVar(env, "XDG_DATA_HOME");
  if (xdg !== null && path.posix.isAbsolute(xdg)) {
    return xdg;
  }

  const homeDir = home();
  if (typeof homeDir === "string" && homeDir !== "") {
    return path.posix.join(homeDir, ".local", "share");
  }

  throw new PathsError(
    "no-base-dir",
    "cannot resolve data_dir: set LOCAL_RAG_HOME or the platform data/config directory",
  );
}

/**
 * The store root — `<data_dir>/local-rag`, the component `StoreLayout::resolve`
 * adds (`crates/core/src/paths/mod.rs:267-270`).
 *
 * @param {NodeJS.ProcessEnv} [env] @param {object} [opts] @returns {string}
 */
function storeRoot(env = process.env, opts = {}) {
  const p = flavour(opts.platform ?? process.platform);
  return p.join(dataDir(env, opts), "local-rag");
}

/**
 * Where this package installs native binaries when it has nowhere better.
 *
 * Keyed by target triple rather than by platform key so two architectures can
 * coexist under one home — a Rosetta shell and a native one on the same Mac
 * resolve different triples and must not overwrite each other's binaries.
 *
 * @param {string} key e.g. "darwin-arm64" @param {NodeJS.ProcessEnv} [env]
 * @param {object} [opts] @returns {string}
 */
function installDir(key, env = process.env, opts = {}) {
  const triple = targetTriple(key);
  if (triple === null) {
    throw new PathsError("unsupported-platform", `no release target for ${key}`);
  }
  const p = flavour(opts.platform ?? process.platform);
  return p.join(storeRoot(env, opts), "bin", triple);
}

module.exports = { PathsError, dataDir, storeRoot, installDir };
