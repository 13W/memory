"use strict";

// Builds synthetic on-disk `node_modules` trees standing in for real
// npm/yarn/pnpm installs, so the resolution logic can be exercised without
// ever running a real `npm install` (hermetic, offline, no registry).

const fs = require("node:fs");
const path = require("node:path");

const DEFAULT_BINARIES = Object.freeze(["local-rag", "local-rag-proxy", "local-rag-hook"]);
const REAL_LAUNCHER_ROOT = path.resolve(__dirname, "..", "..");

/** "@13w/local-rag-darwin-arm64" -> "local-rag-darwin-arm64" */
function shortName(fullName) {
  return fullName.split("/")[1];
}

function writeProductBinaries(binDir, platform, binaries, binaryContents) {
  fs.mkdirSync(binDir, { recursive: true });
  const suffix = platform === "win32" ? ".exe" : "";
  for (const name of binaries) {
    const filePath = path.join(binDir, name + suffix);
    fs.writeFileSync(filePath, binaryContents[name] ?? "");
    fs.chmodSync(filePath, 0o755);
  }
}

/**
 * @param {string} packageDir
 * @param {string} packageName
 * @param {{platform?: string, cpu?: string, binaries?: string[], binaryContents?: Record<string,string>}} [opts]
 */
function writePlatformPackageAt(packageDir, packageName, opts = {}) {
  const platform = opts.platform ?? "linux";
  const cpu = opts.cpu ?? "x64";
  const binaries = opts.binaries ?? DEFAULT_BINARIES;
  const binaryContents = opts.binaryContents ?? {};
  fs.mkdirSync(packageDir, { recursive: true });
  fs.writeFileSync(
    path.join(packageDir, "package.json"),
    JSON.stringify({ name: packageName, version: "0.0.0", os: [platform], cpu: [cpu] }, null, 2),
  );
  writeProductBinaries(path.join(packageDir, "bin"), platform, binaries, binaryContents);
}

/**
 * Copies the real launcher package (`bin/` + `src/` + `package.json`) into
 * `launcherDir`, so subprocess-tier tests can spawn a genuine, standalone
 * copy of `bin/local-rag-mcp.js` against a fixture tree instead of the real
 * checkout (which has no `node_modules` of its own to resolve against).
 * `bin/local-rag-mcp.js`'s own `require("../src/...")` calls stay correct
 * after the copy because both directories move together.
 *
 * @returns {string} absolute path to the copied `bin/local-rag-mcp.js`
 */
function writeLauncherPackageAt(launcherDir) {
  fs.mkdirSync(launcherDir, { recursive: true });
  fs.cpSync(path.join(REAL_LAUNCHER_ROOT, "src"), path.join(launcherDir, "src"), { recursive: true });
  fs.mkdirSync(path.join(launcherDir, "bin"), { recursive: true });
  fs.copyFileSync(
    path.join(REAL_LAUNCHER_ROOT, "bin", "local-rag-mcp.js"),
    path.join(launcherDir, "bin", "local-rag-mcp.js"),
  );
  fs.chmodSync(path.join(launcherDir, "bin", "local-rag-mcp.js"), 0o755);
  fs.copyFileSync(
    path.join(REAL_LAUNCHER_ROOT, "package.json"),
    path.join(launcherDir, "package.json"),
  );
  return path.join(launcherDir, "bin", "local-rag-mcp.js");
}

/**
 * Flat/hoisted layout: the launcher and every requested platform package
 * sit as plain sibling directories directly under one shared
 * `node_modules/@13w/` — the common case for plain npm and yarn classic.
 *
 * @param {string} root - must already exist (pass a fresh `mkdtempSync` dir).
 * @param {Array<{name: string} & Parameters<typeof writePlatformPackageAt>[2]>} platformPackages
 * @returns {{launcherBinFile: string, packageDirs: Record<string,string>}}
 */
function buildFlatLayout(root, platformPackages) {
  const scope = path.join(root, "node_modules", "@13w");
  const launcherBinFile = writeLauncherPackageAt(path.join(scope, "local-rag"));
  const packageDirs = {};
  for (const p of platformPackages) {
    const dir = path.join(scope, shortName(p.name));
    writePlatformPackageAt(dir, p.name, p);
    packageDirs[p.name] = dir;
  }
  return { launcherBinFile, packageDirs };
}

/**
 * Nested/unhoisted npm layout: platform packages live inside the
 * launcher's own private `node_modules`, not the shared top-level one —
 * one of the real layouts npm itself can produce depending on the
 * dependency graph/lockfile.
 *
 * @param {string} root
 * @param {Array<{name: string} & Parameters<typeof writePlatformPackageAt>[2]>} platformPackages
 * @returns {{launcherBinFile: string, packageDirs: Record<string,string>}}
 */
function buildNestedLayout(root, platformPackages) {
  const launcherDir = path.join(root, "node_modules", "@13w", "local-rag");
  const launcherBinFile = writeLauncherPackageAt(launcherDir);
  const nestedScope = path.join(launcherDir, "node_modules", "@13w");
  const packageDirs = {};
  for (const p of platformPackages) {
    const dir = path.join(nestedScope, shortName(p.name));
    writePlatformPackageAt(dir, p.name, p);
    packageDirs[p.name] = dir;
  }
  return { launcherBinFile, packageDirs };
}

/**
 * pnpm-style layout: every package's real files live in a flat,
 * content-addressed-ish `.pnpm` store directory, each with its own
 * *private* `node_modules` holding only its own declared deps, wired
 * together entirely through real symlinks — using absolute symlink targets
 * throughout so there is no hand-counted relative-depth arithmetic to get
 * wrong (a mistake that silently breaks the very thing under test). This is
 * the actual property being proven: `createRequire`'s own directory walk,
 * anchored at the launcher's real symlink-resolved location, must reach
 * the platform package's *private* `node_modules`, exactly like pnpm's own
 * non-hoisting isolation model expects.
 *
 * @param {string} root
 * @param {Array<{name: string} & Parameters<typeof writePlatformPackageAt>[2]>} platformPackages
 * @returns {{launcherBinFile: string, packageDirs: Record<string,string>}}
 */
function buildPnpmLayout(root, platformPackages) {
  const store = path.join(root, "node_modules", ".pnpm");
  const launcherRealDir = path.join(
    store,
    "@13w+local-rag@0.0.0",
    "node_modules",
    "@13w",
    "local-rag",
  );
  writeLauncherPackageAt(launcherRealDir);

  const launcherPrivateScope = path.join(launcherRealDir, "node_modules", "@13w");
  fs.mkdirSync(launcherPrivateScope, { recursive: true });

  const packageDirs = {};
  for (const p of platformPackages) {
    const short = shortName(p.name);
    const realDir = path.join(store, `@13w+${short}@0.0.0`, "node_modules", "@13w", short);
    writePlatformPackageAt(realDir, p.name, p);
    fs.symlinkSync(realDir, path.join(launcherPrivateScope, short), "dir");
    packageDirs[p.name] = realDir;
  }

  const topLevelLink = path.join(root, "node_modules", "@13w", "local-rag");
  fs.mkdirSync(path.dirname(topLevelLink), { recursive: true });
  fs.symlinkSync(launcherRealDir, topLevelLink, "dir");

  return {
    launcherBinFile: path.join(topLevelLink, "bin", "local-rag-mcp.js"),
    packageDirs,
  };
}

module.exports = {
  DEFAULT_BINARIES,
  shortName,
  writePlatformPackageAt,
  writeLauncherPackageAt,
  buildFlatLayout,
  buildNestedLayout,
  buildPnpmLayout,
};
