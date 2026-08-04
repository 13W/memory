"use strict";

// Card requirement: "no writes inside sample repository" — CLAUDE.md's own
// guardrail ("plugin packaging must not modify users' repositories") as a
// structural, executed proof, not a promise. Runs the REAL cargo-built
// `local-rag-hook` binary through this plugin's new JS bootstrap/cache
// layer (`bin/local-rag-hook.js`), with `cwd` pointed at a synthetic
// "sample repository", and asserts that repository's file tree is
// byte-for-byte unchanged — stronger than checking for `.claude/rules/`
// specifically, since it catches *any* unexpected write.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const { buildFlatLayout } = require("../../npm/memory/test/helpers/fixture-layout.js");
const { nativeHookBinaryPath } = require("./helpers/native-hook-binary.js");
const { prepareSpoolDir } = require("./helpers/store-fixture.js");
const { listTree } = require("./helpers/sample-repo.js");

const nativeBin = nativeHookBinaryPath();
const SKIP_REASON = "target/debug/local-rag-hook is not built — run `cargo build -p local-rag-hook` first";

test(
  "the bootstrap+exec chain writes nothing into the sample repository it runs against",
  { skip: !nativeBin && SKIP_REASON },
  () => {
    const platformKey = `${process.platform}-${process.arch}`;
    const root = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-nowrites-")));
    const { launcherBinFile, packageDirs } = buildFlatLayout(root, [
      { name: `@13w/memory-${platformKey}`, platform: process.platform, cpu: process.arch },
    ]);
    // Replace the stub with a real symlink to the actual cargo-built
    // binary — genuine end-to-end coverage, not a stand-in.
    const hookStubPath = path.join(packageDirs[`@13w/memory-${platformKey}`], "bin", "local-rag-hook");
    fs.rmSync(hookStubPath);
    fs.symlinkSync(nativeBin, hookStubPath);
    const hookJsFile = path.join(path.dirname(launcherBinFile), "local-rag-hook.js");

    const sampleRepo = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "lr-sample-repo-")));
    fs.writeFileSync(path.join(sampleRepo, "existing-file.txt"), "hello\n");
    fs.mkdirSync(path.join(sampleRepo, "src"));
    fs.writeFileSync(path.join(sampleRepo, "src", "main.rs"), "fn main() {}\n");
    const beforeTree = listTree(sampleRepo);

    const localRagHome = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-home-"));
    prepareSpoolDir(localRagHome, "sess-nowrites");
    const pluginData = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-data-"));

    const eventJson = JSON.stringify({
      session_id: "sess-nowrites",
      hook_event_name: "SessionStart",
      cwd: sampleRepo,
      source: "startup",
    });

    const result = spawnSync(process.execPath, [hookJsFile, "spool-write"], {
      input: eventJson,
      encoding: "utf8",
      env: { ...process.env, LOCAL_RAG_HOME: localRagHome, CLAUDE_PLUGIN_DATA: pluginData },
      cwd: sampleRepo,
    });
    assert.equal(result.status, 0, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);

    const afterTree = listTree(sampleRepo);
    assert.deepEqual(afterTree, beforeTree, "the sample repository must be byte-for-byte unchanged");
    assert.ok(
      !afterTree.some((p) => p.includes(".claude")),
      "no .claude/ directory of any kind may appear in the sample repository",
    );

    // Confirm this was a real, successful write — not a silently-skipped
    // no-op that would make the "no writes" assertion trivially true.
    const spoolFiles = fs.readdirSync(path.join(localRagHome, "local-rag", "spool", "sess-nowrites"));
    assert.ok(spoolFiles.length > 0, "a real spool segment should have been written to the store");

    fs.rmSync(root, { recursive: true, force: true });
    fs.rmSync(sampleRepo, { recursive: true, force: true });
    fs.rmSync(localRagHome, { recursive: true, force: true });
    fs.rmSync(pluginData, { recursive: true, force: true });
  },
);
