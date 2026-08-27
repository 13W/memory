"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const { resolvePlatformPackage } = require("../src/resolve.js");
const { buildFlatLayout, writeLauncherPackageAt, writePlatformPackageAt } = require("./helpers/fixture-layout.js");
const { waitForStdoutLine, pidIsAlive, waitUntil } = require("./helpers/proc.js");

function mkSpacedTmpRoot() {
  // Directory name itself contains a space and parentheses — the
  // characteristic shape of e.g. "/Users/me/My Projects (work)/...".
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "lr test (spaces)-"));
  return fs.realpathSync(dir);
}

test("resolution works when the whole install tree lives under a path containing spaces", () => {
  const root = mkSpacedTmpRoot();
  assert.match(root, / /, "sanity: the fixture root really does contain a space");

  const { launcherBinFile, packageDirs } = buildFlatLayout(root, [
    { name: "@13w/memory-linux-x64", platform: "linux", cpu: "x64" },
  ]);

  const result = resolvePlatformPackage(launcherBinFile, { platform: "linux", arch: "x64" });
  assert.equal(result.ok, true);
  assert.equal(result.packageDir, packageDirs["@13w/memory-linux-x64"]);

  fs.rmSync(root, { recursive: true, force: true });
});

test("spawning the launcher under a spaced path works with no shell/quoting involved (argv array, not a shell string)", { skip: process.platform === "win32" }, async () => {
  const root = mkSpacedTmpRoot();
  const fakeBinarySrc = fs.readFileSync(path.join(__dirname, "helpers", "fake-binary.js"), "utf8");
  const { launcherBinFile, packageDirs } = buildFlatLayout(root, [
    {
      name: `@13w/memory-${process.platform}-${process.arch}`,
      platform: process.platform,
      cpu: process.arch,
      binaryContents: { "local-rag-proxy": fakeBinarySrc },
    },
  ]);

  // T22-12 deleted `bin/local-rag-mcp.js`, so `launcherBinFile` is now the
  // shipping stub, which resolves through `locate.js` rather than through a
  // platform package. `LOCAL_RAG_BIN_DIR` points it at the fixture's binaries;
  // `LOCAL_RAG_HOME` keeps the developer's real cache out of the answer.
  const binDir = path.join(packageDirs[`@13w/memory-${process.platform}-${process.arch}`], "bin");
  const launcher = spawn(process.execPath, [launcherBinFile], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      LOCAL_RAG_BIN_DIR: binDir,
      LOCAL_RAG_HOME: path.join(root, "empty-home"),
      LOCAL_RAG_RELEASE_BASE_URL: "http://127.0.0.1:1/releases",
    },
  });
  const { line } = await waitForStdoutLine(launcher, (l) => l.startsWith("READY "));
  const childPid = Number(/pid=(\d+)/.exec(line)[1]);

  process.kill(launcher.pid, "SIGTERM");
  const [code] = await new Promise((resolve) => launcher.on("exit", (c, s) => resolve([c, s])));
  assert.equal(code, 0);
  await waitUntil(() => !pidIsAlive(childPid), { description: "grandchild exit under a spaced path" });

  fs.rmSync(root, { recursive: true, force: true });
});

test("resolution works when the launcher's own install location is itself reached only via a symlink (not just the platform package)", () => {
  const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "lr-symlink-launcher-")));

  const realLauncherDir = path.join(root, "real-store", "memory");
  writeLauncherPackageAt(realLauncherDir);

  const platformDir = path.join(root, "real-store", "memory-linux-x64");
  writePlatformPackageAt(platformDir, "@13w/memory-linux-x64", { platform: "linux", cpu: "x64" });

  // The launcher's own private node_modules resolves the platform package
  // through a symlink (pnpm-shaped), AND the launcher itself is reached
  // through a second, independent top-level symlink.
  fs.mkdirSync(path.join(realLauncherDir, "node_modules", "@13w"), { recursive: true });
  fs.symlinkSync(
    platformDir,
    path.join(realLauncherDir, "node_modules", "@13w", "memory-linux-x64"),
    "dir",
  );

  const topLevelLink = path.join(root, "node_modules", "@13w", "memory");
  fs.mkdirSync(path.dirname(topLevelLink), { recursive: true });
  fs.symlinkSync(realLauncherDir, topLevelLink, "dir");

  const launcherBinFile = path.join(topLevelLink, "bin", "local-rag-mcp.js");
  const result = resolvePlatformPackage(launcherBinFile, { platform: "linux", arch: "x64" });
  assert.equal(result.ok, true);
  assert.equal(result.packageDir, platformDir);

  fs.rmSync(root, { recursive: true, force: true });
});

test("a symlinked launcher location combined with a spaced path resolves correctly (both edge cases at once)", () => {
  const root = mkSpacedTmpRoot();

  const realLauncherDir = path.join(root, "actual location", "memory");
  writeLauncherPackageAt(realLauncherDir);
  const platformDir = path.join(root, "actual location", "memory-darwin-arm64");
  writePlatformPackageAt(platformDir, "@13w/memory-darwin-arm64", { platform: "darwin", cpu: "arm64" });
  fs.mkdirSync(path.join(realLauncherDir, "node_modules", "@13w"), { recursive: true });
  fs.symlinkSync(
    platformDir,
    path.join(realLauncherDir, "node_modules", "@13w", "memory-darwin-arm64"),
    "dir",
  );

  const link = path.join(root, "linked (install)", "memory");
  fs.mkdirSync(path.dirname(link), { recursive: true });
  fs.symlinkSync(realLauncherDir, link, "dir");

  const launcherBinFile = path.join(link, "bin", "local-rag-mcp.js");
  const result = resolvePlatformPackage(launcherBinFile, { platform: "darwin", arch: "arm64" });
  assert.equal(result.ok, true);
  assert.equal(result.packageDir, platformDir);

  fs.rmSync(root, { recursive: true, force: true });
});
