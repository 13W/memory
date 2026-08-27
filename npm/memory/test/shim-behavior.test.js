"use strict";

// What the stubs do when the native binary is not where the command points
// (T22-10). Real processes throughout: the whole subject is exit codes, which
// stream got written, and whether anything reached the network — none of which
// an in-process call can answer, because these entry points end in
// `process.exit`.
//
// Every spawn pins `LOCAL_RAG_HOME` and `LOCAL_RAG_RELEASE_BASE_URL`. Without
// the first, the resolver's last rung is the developer's own cache; without the
// second, a stub that heals before reporting reaches the real GitHub.

if (process.platform === "win32") {
  const { test } = require("node:test");
  const why = "stub behaviour is asserted on POSIX only (exit codes and mode bits)";
  test(why, { skip: true }, () => {});
  return;
}

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");
const { spawn } = require("node:child_process");

const { PRODUCT_BINARIES, assetName, sidecarName, executableName } = require("../src/release.js");
const { platformKey, targetTriple } = require("../src/platform.js");
const { readManifest } = require("../src/install.js");
const { writeLauncherPackageAt } = require("./helpers/fixture-layout.js");
const { startFixtureRelease } = require("./helpers/fixture-server.js");
const { mkTmpRoot } = require("./helpers/tmp.js");
const { buildTar, buildZip } = require("./helpers/archive-fixtures.js");

const KEY = platformKey();
const PLATFORM = process.platform;
const REQUIRED = PRODUCT_BINARIES.filter((b) => b.required).map((b) => b.name);

/** A directory of executables that report what they were given. */
function writeEchoBinaries(dir, names, { exitCode = 0 } = {}) {
  fs.mkdirSync(dir, { recursive: true });
  for (const name of names) {
    const file = path.join(dir, executableName(name, PLATFORM));
    fs.writeFileSync(file, `#!/bin/sh\necho "RAN ${name} $*"\nexit ${exitCode}\n`);
    fs.chmodSync(file, 0o755);
  }
  return dir;
}

/** A standalone copy of this package, with its stubs still in place. */
function stubPackage(prefix) {
  const root = mkTmpRoot(`lr-shim-${prefix}-`);
  const pkg = path.join(root, "pkg");
  writeLauncherPackageAt(pkg);
  return { root, pkg, stub: (name) => path.join(pkg, "bin", name) };
}

function run(file, args = [], env = {}) {
  const child = spawn(process.execPath, [file, ...args], {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      LOCAL_RAG_HOME: path.join(path.dirname(file), "..", "..", "empty-home"),
      LOCAL_RAG_RELEASE_BASE_URL: "http://127.0.0.1:1/releases",
      ...env,
    },
  });
  const out = [];
  const err = [];
  child.stdout.on("data", (c) => out.push(c));
  child.stderr.on("data", (c) => err.push(c));
  return new Promise((resolve) => {
    child.on("exit", (code, signal) =>
      resolve({
        code,
        signal,
        stdout: Buffer.concat(out).toString("utf8"),
        stderr: Buffer.concat(err).toString("utf8"),
      }),
    );
  });
}

test("a resolved binary gets the argv it was given, and its exit code comes back", async (t) => {
  const s = stubPackage("argv");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const binDir = writeEchoBinaries(path.join(s.root, "bin"), REQUIRED);

  const proxy = await run(s.stub("local-rag-proxy"), ["--flag", "a b"], {
    LOCAL_RAG_BIN_DIR: binDir,
  });
  assert.equal(proxy.code, 0);
  assert.match(proxy.stdout, /RAN local-rag-proxy --flag a b/);

  const hook = await run(s.stub("local-rag-hook"), ["spool-write"], {
    LOCAL_RAG_BIN_DIR: binDir,
  });
  assert.equal(hook.code, 0);
  assert.match(hook.stdout, /RAN local-rag-hook spool-write/);
});

test("a non-zero exit from the binary is passed through, not swallowed", async (t) => {
  const s = stubPackage("exitcode");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const binDir = writeEchoBinaries(path.join(s.root, "bin"), REQUIRED, { exitCode: 42 });

  const proxy = await run(s.stub("local-rag-proxy"), [], { LOCAL_RAG_BIN_DIR: binDir });
  assert.equal(proxy.code, 42);

  // The hook passes it through too rather than hardcoding 0: `local-rag-hook
  // version` has to be able to report a real code, and only *our own* failures
  // are forced to 0.
  const hook = await run(s.stub("local-rag-hook"), [], { LOCAL_RAG_BIN_DIR: binDir });
  assert.equal(hook.code, 42);
});

test("the hook exits 0 on every failure, and the proxy exits 1 on the same set", async (t) => {
  const s = stubPackage("failmodes");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const empty = path.join(s.root, "empty");
  fs.mkdirSync(empty, { recursive: true });

  const cases = [
    ["an override with nothing in it", { LOCAL_RAG_BIN_DIR: empty }],
    ["a platform with no release", { LOCAL_RAG_BIN_DIR: empty, LOCAL_RAG_HOME: s.root }],
  ];
  for (const [label, env] of cases) {
    const hook = await run(s.stub("local-rag-hook"), [], env);
    assert.equal(hook.code, 0, `${label}: the hook contract is always exit 0 — ${hook.stderr}`);
    assert.equal(hook.stdout, "", `${label}: the hook writes nothing to stdout`);

    const proxy = await run(s.stub("local-rag-proxy"), [], env);
    assert.equal(proxy.code, 1, `${label}: a failed MCP server must be visible to the client`);
    assert.equal(proxy.stdout, "", `${label}: stdout is the JSON-RPC stream`);
    assert.match(proxy.stderr, /local-rag:/, label);
  }
});

test("an override that is missing is never healed, by either stub", async (t) => {
  // ADR-0013 gives `LOCAL_RAG_BIN_DIR` to air-gapped installs, "which wins over
  // everything and never downloads". A stub that quietly fetched when the
  // override was wrong would make the variable a suggestion.
  const server = await startFixtureRelease({ tag: "1.0.0", assets: {} });
  t.after(() => server.close());
  const s = stubPackage("no-heal");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const empty = path.join(s.root, "empty");
  fs.mkdirSync(empty, { recursive: true });
  const env = { LOCAL_RAG_BIN_DIR: empty, LOCAL_RAG_RELEASE_BASE_URL: server.origin };

  const proxy = await run(s.stub("local-rag-proxy"), [], env);
  assert.equal(proxy.code, 1);
  assert.match(proxy.stderr, /never silently ignored/);

  const hook = await run(s.stub("local-rag-hook"), [], env);
  assert.equal(hook.code, 0);

  assert.equal(server.requestCount(), 0, "neither stub asked the release for anything");
});

test("a checkout with nothing built is reported, not downloaded around", async (t) => {
  const server = await startFixtureRelease({ tag: "1.0.0", assets: {} });
  t.after(() => server.close());
  const root = mkTmpRoot("lr-shim-checkout-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const pkg = path.join(root, "npm", "memory");
  fs.mkdirSync(pkg, { recursive: true });
  writeLauncherPackageAt(pkg);
  fs.writeFileSync(path.join(root, "Cargo.toml"), "[workspace]\n");
  fs.writeFileSync(path.join(root, "dist-workspace.toml"), "[dist]\n");
  const home = path.join(root, "home");
  const env = { LOCAL_RAG_RELEASE_BASE_URL: server.origin, LOCAL_RAG_HOME: home };

  const proxy = await run(path.join(pkg, "bin", "local-rag-proxy"), [], env);
  assert.equal(proxy.code, 1);
  assert.match(proxy.stderr, /cargo build --release/);
  assert.equal(server.requestCount(), 0, "the local build is the point; nothing was fetched");
});

test("with --ignore-scripts the MCP stub heals on first use, then runs", async (t) => {
  // The card's acceptance in one test: `postinstall` never ran, so nothing is
  // installed, and the command still works.
  const assets = {};
  for (const binary of PRODUCT_BINARIES) {
    const asset = assetName(binary.name, KEY);
    const build = asset.endsWith(".zip") ? buildZip : buildTar;
    const archive = build([
      {
        name: executableName(binary.name, PLATFORM),
        data: `#!/bin/sh\necho "HEALED ${binary.name}"\n`,
      },
    ]);
    const digest = crypto.createHash("sha256").update(archive).digest("hex");
    assets[asset] = archive;
    assets[sidecarName(asset)] = `${digest} *${asset}\n`;
  }
  const server = await startFixtureRelease({ tag: "6.1.0", assets });
  t.after(() => server.close());
  const s = stubPackage("heal");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const home = path.join(s.root, "home");
  const env = { LOCAL_RAG_HOME: home, LOCAL_RAG_RELEASE_BASE_URL: server.origin };

  const first = await run(s.stub("local-rag-proxy"), [], env);
  assert.equal(first.code, 0, first.stderr);
  assert.match(first.stdout, /HEALED local-rag-proxy/);

  const cache = path.join(home, "local-rag", "bin", targetTriple(KEY));
  assert.equal(readManifest(cache).tag, "6.1.0");

  // And the second run is free: the manifest is there, so `--if-needed` asks
  // the release for nothing.
  const before = server.requestCount();
  const second = await run(s.stub("local-rag-proxy"), [], env);
  assert.equal(second.code, 0, second.stderr);
  assert.equal(server.requestCount(), before, "a healed install stops touching the network");
});

test("the hook stub stays fail-open even when its own module cannot be loaded", async (t) => {
  // `11 §3.1` `[FIXED]` covers the whole command a hooks.json entry runs, and a
  // top-level `require` that throws is not fail-open — the shape the previous
  // shim had. Breaking the module is the only way to reach that path.
  const s = stubPackage("broken");
  t.after(() => fs.rmSync(s.root, { recursive: true, force: true }));
  const broken = "throw new Error('deliberately broken');\n";
  fs.writeFileSync(path.join(s.pkg, "src", "shim.js"), broken);

  const hook = await run(s.stub("local-rag-hook"), []);
  assert.equal(hook.code, 0, "a broken install must not fail a hook");
  assert.equal(hook.stdout, "");
  assert.match(hook.stderr, /deliberately broken/);

  const proxy = await run(s.stub("local-rag-proxy"), []);
  assert.equal(proxy.code, 1, "the same breakage is correctly fatal for the MCP server");
  assert.equal(proxy.stdout, "");
});
