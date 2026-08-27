"use strict";

// Putting the native binary where the command already points.
//
// npm links a package's `bin` entries **before** it runs `postinstall` —
// verified in its own source, `@npmcli/arborist/lib/arborist/rebuild.js`:
// `#linkAllBins()` runs, and only then `#runScripts('postinstall')`. So by the
// time this module runs, `.bin/local-rag-proxy` already exists and already
// points at `<pkg>/bin/local-rag-proxy`. Replacing the file at that path leaves
// the link valid and takes Node out of the hot path entirely: the command execs
// a native binary directly.
//
// WHY `linkSync` + `renameSync`, AND NEVER A WRITE.
//
// Any file inside `node_modules` may be a hard link into a shared,
// content-addressed store — pnpm's, yarn's `nmMode: hardlinks-*`, npm's own
// `--install-links`. `writeFileSync` opens with `O_TRUNC` and rewrites *that
// inode*, so it would reach through the link and rewrite the store's copy,
// which every other project on the machine shares and whose filename is the
// digest of what it used to contain. Reproduced before this module was written:
// a `writeFileSync` through a hard link left the store copy holding the new
// bytes. `rename(2)` replaces a *directory entry* instead — the old inode keeps
// its content and merely loses one link — so it is the only replace that cannot
// leak through one.
//
// WHY THIS REFUSES TO RUN OUTSIDE npm, which is more than the card asked for.
//
// The design assumes the command entry is a **symlink**. That is unconditional
// for npm on POSIX (`bin-links/lib/link-bins.js` picks `link-bin.js` unless
// `isWindows`), and it is false for everyone else:
//
//   - Windows: npm writes `.cmd` and `.ps1` wrappers holding `node "%~dp0..."`.
//   - Yarn: its own wrappers, and under PnP there is no `node_modules` at all.
//   - **pnpm with its default `isolated` linker: `cmd-shim` regular files.**
//     This one is the trap, and it is invisible from the card's win32/yarn
//     rule. `cmd-shim` inspects the target's shebang *when it writes the shim*
//     and bakes the decision in. Because bins are linked before install
//     scripts, that inspection sees the JS stub and writes `exec node
//     .../bin/local-rag "$@"` permanently. Replace the target afterwards and
//     the wrapper hands a Mach-O binary to Node. Both shapes are on this
//     machine to compare: a pnpm shim for a JS target contains `exec node …`,
//     one for a native target contains `exec "$basedir/…/pnpm"` and no `node`.
//
// So replacement happens only where the linker is known to have made a
// symlink, and everywhere else the JS stub stays and resolves at run time. That
// costs pnpm and Yarn users one Node start per invocation and costs nobody a
// broken command. It also costs less than it sounds: pnpm 10+ gates dependency
// build scripts behind `pnpm approve-builds`, so `postinstall` does not run
// there by default at all — those users are on the lazy path either way.

const fs = require("node:fs");
const path = require("node:path");

const { PRODUCT_BINARIES, executableName } = require("./release");

const DISABLE_VAR = "LOCAL_RAG_NO_BIN_REPLACE";

/**
 * Which package manager is running us, from the one signal all of them set.
 *
 * @param {NodeJS.ProcessEnv} env
 * @returns {string|null} e.g. "npm", "pnpm", "yarn"; null when nothing said.
 */
function packageManagerFrom(env) {
  const agent = env.npm_config_user_agent;
  if (typeof agent !== "string" || agent === "") return null;
  const name = /^([a-z][a-z0-9-]*)\//i.exec(agent.trim());
  return name === null ? null : name[1].toLowerCase();
}

/**
 * Whether replacement is safe here, and if not, why.
 *
 * @returns {{ok: true} | {ok: false, reason: string}}
 */
function replacementIsSafe(env = process.env, platform = process.platform) {
  if (env[DISABLE_VAR] !== undefined && env[DISABLE_VAR] !== "") {
    return { ok: false, reason: `${DISABLE_VAR} is set` };
  }
  if (platform === "win32") {
    return { ok: false, reason: "npm writes .cmd/.ps1 wrappers on Windows, not symlinks" };
  }
  const manager = packageManagerFrom(env);
  if (manager === null) {
    return { ok: false, reason: "no npm_config_user_agent: not running under a package manager" };
  }
  if (manager !== "npm") {
    return {
      ok: false,
      reason: `${manager} does not link bins as symlinks; its wrapper would keep calling node`,
    };
  }
  return { ok: true };
}

/**
 * Replace one shim with a hard link to `native`.
 *
 * @returns {"replaced"|"same"|"missing"|"failed"}
 */
function replaceOne(native, shim) {
  if (!fs.existsSync(native)) return "missing";
  try {
    const a = fs.statSync(native);
    const b = fs.statSync(shim);
    // Already the same file: a re-run of `postinstall` must be a no-op rather
    // than a churn of temporary links.
    if (a.dev === b.dev && a.ino === b.ino) return "same";
  } catch {
    // No shim there at all is fine — the rename below creates one.
  }
  const staging = `${shim}.new-${process.pid}`;
  try {
    fs.rmSync(staging, { force: true });
    fs.linkSync(native, staging);
    fs.renameSync(staging, shim);
    return "replaced";
  } catch {
    // The commonest cause is EXDEV — the cache and `node_modules` on different
    // filesystems — and the answer is the same for all of them: leave the stub,
    // which finds the cache by itself. Not reproducible on this machine, where
    // both live on one volume.
    try {
      fs.rmSync(staging, { force: true });
    } catch {
      // Nothing further to do about the leftover.
    }
    return "failed";
  }
}

/**
 * Point every command at the native binary installed in `sourceDir`.
 *
 * @param {string} packageDir this package's root
 * @param {string} sourceDir where `install.js` put the binaries
 * @param {object} [opts] `env`, `platform`, `binaries`, `log`
 * @returns {{skipped: boolean, reason?: string, replaced: string[], failed: string[]}}
 */
function replaceShims(packageDir, sourceDir, opts = {}) {
  const env = opts.env ?? process.env;
  const platform = opts.platform ?? process.platform;
  const binaries = opts.binaries ?? PRODUCT_BINARIES;
  const log = opts.log ?? (() => {});

  const safe = replacementIsSafe(env, platform);
  if (!safe.ok) {
    log(`keeping the Node stubs: ${safe.reason}`);
    return { skipped: true, reason: safe.reason, replaced: [], failed: [] };
  }

  const replaced = [];
  const failed = [];
  for (const binary of binaries) {
    const name = executableName(binary.name, platform);
    const outcome = replaceOne(path.join(sourceDir, name), path.join(packageDir, "bin", name));
    if (outcome === "replaced" || outcome === "same") replaced.push(binary.name);
    else if (outcome === "failed") failed.push(binary.name);
  }
  if (replaced.length > 0) log(`${replaced.length} commands now run the native binary directly`);
  return { skipped: false, replaced, failed };
}

module.exports = {
  DISABLE_VAR,
  packageManagerFrom,
  replacementIsSafe,
  replaceOne,
  replaceShims,
};
