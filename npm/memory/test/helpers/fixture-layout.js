"use strict";

// Builds synthetic on-disk trees for the subprocess-tier tests, so a real
// entrypoint can be spawned against a fixture without ever running a real
// `npm install` (hermetic, offline, no registry).
//
// WHAT "PACKAGE" MEANS HERE CHANGED IN T22-11, and the names below outlived
// it. This helper was built for `resolve.js`, which walked `node_modules` to
// find a per-platform npm package; ADR-0013 deleted that whole channel, and
// with it `buildNestedLayout` and `buildPnpmLayout`, whose only subject was
// which tree shapes that walk had to survive. What survives is the plain part:
// a directory holding the required binaries. `writePlatformPackageAt` still
// writes one, and its callers still hand it a scoped name — but the name is
// now a fixture directory label, not a package anybody could install.

const fs = require("node:fs");
const path = require("node:path");

const DEFAULT_BINARIES = Object.freeze(["local-rag", "local-rag-proxy", "local-rag-hook"]);
const REAL_LAUNCHER_ROOT = path.resolve(__dirname, "..", "..");

/** "@13w/some-name" -> "some-name" — a scope-stripping helper, nothing more. */
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
 * Every entrypoint's own `require("../src/...")` calls stay correct after the
 * copy because the directories move together; copying whole directories (not
 * named files) means a new entrypoint needs no change here, mirroring
 * `package.json`'s own `files: ["bin", "src", "scripts"]` allowlist.
 *
 * `scripts/` is copied for the same reason as the other two, and T22-10 is why
 * it had to start being: the shims' repair path spawns `scripts/install.js`,
 * so a fixture tree without it would exercise a launcher that can never heal —
 * passing for the wrong reason.
 *
 * @returns {string} absolute path to the copied `bin/local-rag-proxy` — the
 *   stub that actually ships since T22-10. Most callers want only its
 *   directory; T22-12 deleted `bin/local-rag-mcp.js`, so returning that name
 *   would hand back a path to a file that is not there.
 */
function writeLauncherPackageAt(launcherDir) {
  fs.mkdirSync(launcherDir, { recursive: true });
  fs.cpSync(path.join(REAL_LAUNCHER_ROOT, "src"), path.join(launcherDir, "src"), { recursive: true });
  fs.cpSync(path.join(REAL_LAUNCHER_ROOT, "bin"), path.join(launcherDir, "bin"), { recursive: true });
  fs.cpSync(path.join(REAL_LAUNCHER_ROOT, "scripts"), path.join(launcherDir, "scripts"), {
    recursive: true,
  });
  for (const name of fs.readdirSync(path.join(launcherDir, "bin"))) {
    fs.chmodSync(path.join(launcherDir, "bin", name), 0o755);
  }
  fs.copyFileSync(
    path.join(REAL_LAUNCHER_ROOT, "package.json"),
    path.join(launcherDir, "package.json"),
  );
  return path.join(launcherDir, "bin", "local-rag-proxy");
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
  const launcherBinFile = writeLauncherPackageAt(path.join(scope, "memory"));
  const packageDirs = {};
  for (const p of platformPackages) {
    const dir = path.join(scope, shortName(p.name));
    writePlatformPackageAt(dir, p.name, p);
    packageDirs[p.name] = dir;
  }
  return { launcherBinFile, packageDirs };
}

module.exports = {
  writePlatformPackageAt,
  writeLauncherPackageAt,
  buildFlatLayout,
};
