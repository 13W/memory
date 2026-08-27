"use strict";

// Card requirement: "no writes inside sample repository" — CLAUDE.md's own
// guardrail ("plugin packaging must not modify users' repositories") as a
// structural, executed proof, not a promise.
//
// Runs the REAL cargo-built `local-rag-hook` through the REAL `hooks.json`
// command line (T22-13; previously through the JS bootstrap/cache layer that
// ADR-0013 retired), with `cwd` pointed at a synthetic "sample repository",
// and asserts that repository is unchanged — stronger than checking for
// `.claude/rules/` specifically, since it catches *any* unexpected write.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const { mkTmpRoot } = require("./helpers/tmp.js");
const { nativeBinaryPath, REPO_ROOT } = require("./helpers/native-binary.js");
const { prepareSpoolDir } = require("./helpers/store-fixture.js");
const { listTree } = require("./helpers/sample-repo.js");

const PLUGIN_ROOT = path.join(REPO_ROOT, "plugin");
const nativeBin = nativeBinaryPath("local-rag-hook");
const SKIP_REASON = "target/debug/local-rag-hook is not built — run `cargo build -p local-rag-hook` first";

test(
  "the shipped hooks.json command writes nothing into the repository it runs against",
  { skip: !nativeBin && SKIP_REASON },
  () => {
    // The command as Claude Code would run it, read out of the file rather
    // than retyped — a hand-copied line would drift from the shipped one and
    // this test would then vouch for something that is not installed.
    const hooks = JSON.parse(fs.readFileSync(path.join(PLUGIN_ROOT, "hooks", "hooks.json"), "utf8"));
    const command = hooks.hooks.SessionStart[0].hooks[0].command;

    // `LOCAL_RAG_BIN_DIR` rather than the test seam: this is the production
    // first rung of 13 §2's order, and using it means the resolution under
    // test is one a user can actually reproduce.
    const binDir = mkTmpRoot("lr-nowrites-bin-");
    fs.symlinkSync(nativeBin, path.join(binDir, "local-rag-hook"));

    const sampleRepo = mkTmpRoot("lr-sample-repo-");
    fs.writeFileSync(path.join(sampleRepo, "existing-file.txt"), "hello\n");
    fs.mkdirSync(path.join(sampleRepo, "src"));
    fs.writeFileSync(path.join(sampleRepo, "src", "main.rs"), "fn main() {}\n");
    const beforeTree = listTree(sampleRepo);

    const localRagHome = mkTmpRoot("lr-nowrites-home-");
    prepareSpoolDir(localRagHome, "sess-nowrites");
    // ADR-0013 Decision 3 retired the `${CLAUDE_PLUGIN_DATA}/bin` cache. This
    // run uses the real native binary rather than a stub, so it is the strongest
    // place to prove the tier stays gone: a cache keyed by path is what pointed
    // a stale proxy at a stale daemon (D-103). `no-network.test.js` asserts the
    // same for the stubbed MCP channel.
    const pluginData = mkTmpRoot("lr-nowrites-plugindata-");

    const result = spawnSync("/bin/sh", ["-c", command], {
      input: JSON.stringify({
        session_id: "sess-nowrites",
        hook_event_name: "SessionStart",
        cwd: sampleRepo,
        source: "startup",
      }),
      encoding: "utf8",
      cwd: sampleRepo,
      env: {
        PATH: "/usr/bin:/bin",
        HOME: mkTmpRoot("lr-nowrites-fakehome-"),
        CLAUDE_PLUGIN_ROOT: PLUGIN_ROOT,
        LOCAL_RAG_BIN_DIR: binDir,
        LOCAL_RAG_HOME: localRagHome,
        CLAUDE_PLUGIN_DATA: pluginData,
      },
    });
    assert.equal(result.status, 0, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);

    const afterTree = listTree(sampleRepo);
    assert.deepEqual(afterTree, beforeTree, "the sample repository must be unchanged");
    assert.ok(
      !afterTree.some((e) => e.path.includes(".claude")),
      "no .claude/ directory of any kind may appear in the sample repository",
    );

    // Confirm this was a real, successful write — not a silently-skipped
    // no-op that would make the "no writes" assertion trivially true. It also
    // proves the resolver reached the native binary rather than the notice.
    const spoolFiles = fs.readdirSync(path.join(localRagHome, "local-rag", "spool", "sess-nowrites"));
    assert.ok(spoolFiles.length > 0, "a real spool segment should have been written to the store");
    assert.doesNotMatch(result.stdout, /not installed/, "the binary ran; the notice must not appear");
    assert.deepEqual(fs.readdirSync(pluginData), [], "the retired ${CLAUDE_PLUGIN_DATA} cache must stay empty");
  },
);

test("the snapshot this test trusts can actually see an in-place rewrite", () => {
  // Not decoration. The assertion above says "unchanged", and it is only as
  // strong as `listTree`. Until T22-13 that snapshot was a list of names, so
  // a rewrite of an existing file — same name, no new entry — would have
  // passed it silently; the comment claiming "byte-for-byte" was writing a
  // cheque the helper could not cash. These two cases are what the added
  // `size` and `mtimeMs` fields buy, checked directly rather than assumed.
  const dir = mkTmpRoot("lr-snapshot-selfcheck-");
  const file = path.join(dir, "existing-file.txt");
  fs.writeFileSync(file, "hello\n");
  const before = listTree(dir);

  fs.writeFileSync(file, "hello, and then some more\n");
  assert.notDeepEqual(listTree(dir), before, "a length-changing rewrite must show up");

  // Identical bytes, different mtime — `utimesSync` rather than a second
  // write, so this does not depend on the filesystem's timestamp resolution.
  fs.writeFileSync(file, "hello\n");
  fs.utimesSync(file, new Date(0), new Date(0));
  assert.notDeepEqual(listTree(dir), before, "a same-size rewrite must show up too");

  fs.rmSync(dir, { recursive: true, force: true });
});
