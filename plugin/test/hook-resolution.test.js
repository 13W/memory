"use strict";

// T22-13 — the hook half of ADR-0013's "resolve an executable, download
// nothing". Every assertion here runs the SEVEN COMMANDS AS `hooks.json`
// SPELLS THEM, read out of that file rather than retyped, through a real
// `sh -c`. A test that invoked the resolver directly would prove the script
// works and say nothing about whether the plugin ships a line that uses it.
//
// The contract under test is `11 §3.1` `[FIXED]` — seven events, always exit
// 0 — and its `11 §3.2` `[SPEC, ADR-0013]` clause: when the binary cannot be
// resolved, `SessionStart` states the situation through `additionalContext`
// and the other six stay silent.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const { mkTmpRoot } = require("./helpers/tmp.js");
const { candidateBinDirs } = require("../bin/local-rag-mcp-launcher.js");

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const PLUGIN_ROOT = path.join(REPO_ROOT, "plugin");
const RESOLVER = path.join(PLUGIN_ROOT, "bin", "local-rag-resolve-hook.sh");
const GOLDEN_FILE = path.join(PLUGIN_ROOT, "hooks", "not-installed.json");

const SPEC_11_3_1_EVENTS = [
  "SessionStart",
  "UserPromptSubmit",
  "PostToolUse",
  "PostToolUseFailure",
  "Stop",
  "SubagentStop",
  "SessionEnd",
];

/** The command string Claude Code would run for `event`, verbatim. */
function hookCommand(event) {
  const hooks = JSON.parse(fs.readFileSync(path.join(PLUGIN_ROOT, "hooks", "hooks.json"), "utf8"));
  return hooks.hooks[event][0].hooks[0].command;
}

/**
 * A deliberately barren environment. `LOCAL_RAG_TEST_BIN_DIRS` replaces the
 * whole candidate list rather than extending it, so the developer's real
 * `PATH` cannot decide any outcome below — "it resolved" always means "it
 * resolved where this test put it".
 */
function poisonedEnv(binDirs, extra = {}) {
  return {
    PATH: "/usr/bin:/bin",
    HOME: "/nonexistent-home-for-tests",
    CLAUDE_PLUGIN_ROOT: PLUGIN_ROOT,
    LOCAL_RAG_TEST_BIN_DIRS: binDirs.join(path.delimiter),
    ...extra,
  };
}

function runHook(event, binDirs, opts = {}) {
  return spawnSync("/bin/sh", ["-c", hookCommand(event)], {
    input: opts.input ?? "{}",
    encoding: "utf8",
    env: poisonedEnv(binDirs, opts.extraEnv),
  });
}

/**
 * A stand-in for the native `local-rag-hook` that reports what it received.
 * Real enough for what is under test here — this file is about which file
 * gets exec'd and what reaches it, not about spool semantics.
 */
function installFakeHook(dir, marker) {
  fs.mkdirSync(dir, { recursive: true });
  const p = path.join(dir, "local-rag-hook");
  fs.writeFileSync(p, `#!/bin/sh\nprintf '${marker} argv=%s stdin=' "$*"\ncat\nprintf '\\n'\n`);
  fs.chmodSync(p, 0o755);
  return p;
}

test("all seven commands exit 0 when nothing resolves — spec 11 §3.1 [FIXED]", () => {
  const empty = mkTmpRoot("lr-hook-empty-");
  for (const event of SPEC_11_3_1_EVENTS) {
    const r = runHook(event, [empty]);
    assert.equal(r.status, 0, `${event} must exit 0, got ${r.status} (stderr: ${r.stderr})`);
  }
});

test("SessionStart is the only event that speaks when the binary is missing", () => {
  const empty = mkTmpRoot("lr-hook-empty-");
  const golden = fs.readFileSync(GOLDEN_FILE, "utf8");

  const start = runHook("SessionStart", [empty]);
  // Byte-exact, not "contains": this is the same envelope
  // `local_rag_hook::recall::print_hook_output` emits, and a near-miss would
  // be a JSON object Claude Code parses into the wrong shape.
  assert.equal(start.stdout, golden);

  for (const event of SPEC_11_3_1_EVENTS.filter((e) => e !== "SessionStart")) {
    const r = runHook(event, [empty]);
    assert.equal(r.stdout, "", `${event} must print nothing at all, got ${JSON.stringify(r.stdout)}`);
  }
});

test("SessionStart says nothing of its own once the binary resolves", () => {
  const dir = mkTmpRoot("lr-hook-found-");
  installFakeHook(dir, "REAL-HOOK");
  const r = runHook("SessionStart", [dir]);
  assert.equal(r.status, 0);
  // stdout belongs entirely to the binary. Not merely "the notice is absent":
  // the notice's presence alongside real output would be the actual failure
  // mode, since Claude Code would then read two envelopes.
  assert.match(r.stdout, /^REAL-HOOK argv=spool-write /);
  assert.doesNotMatch(r.stdout, /not installed/);
});

test("the stdin a hook event arrives on reaches the exec'd binary byte-for-byte", () => {
  // The one hazard of building the candidate list in shell: it uses a
  // command substitution and a pipeline before `exec`, and the event is on
  // this process's stdin the whole time. Measured, not assumed.
  const dir = mkTmpRoot("lr-hook-stdin-");
  installFakeHook(dir, "REAL-HOOK");
  const event = JSON.stringify({
    session_id: "sess-stdin",
    hook_event_name: "SessionStart",
    cwd: "/tmp",
    source: "startup",
  });
  const r = runHook("SessionStart", [dir], { input: event });
  assert.equal(r.status, 0);
  assert.equal(r.stdout, `REAL-HOOK argv=spool-write stdin=${event}\n`);
});

test("the resolver itself exits 127 on a miss — hooks.json's `|| true` is what makes it 0", () => {
  // The split is deliberate and worth pinning: a script that swallowed its
  // own failure could not tell SessionStart there was nothing to run.
  const empty = mkTmpRoot("lr-hook-empty-");
  const r = spawnSync(RESOLVER, ["spool-write"], {
    encoding: "utf8",
    input: "{}",
    env: poisonedEnv([empty]),
  });
  assert.equal(r.status, 127);
});

test("resolution takes the first directory in order, and skips a non-executable file", () => {
  const first = mkTmpRoot("lr-hook-first-");
  const second = mkTmpRoot("lr-hook-second-");
  installFakeHook(first, "FIRST");
  installFakeHook(second, "SECOND");
  assert.match(runHook("Stop", [first, second]).stdout, /^FIRST /);
  assert.match(runHook("Stop", [second, first]).stdout, /^SECOND /);

  // Present but not executable is not an installation.
  fs.chmodSync(path.join(first, "local-rag-hook"), 0o644);
  assert.match(runHook("Stop", [first, second]).stdout, /^SECOND /);
});

test("no co-location requirement: the hook resolves without `local-rag` beside it", () => {
  // Unlike the MCP launcher, which insists the daemon sit beside the proxy
  // (13 §4, `connect.rs:55`). The hook appends to the spool and talks to the
  // daemon over a socket; demanding a full set here would refuse an install
  // that is complete for this purpose.
  const dir = mkTmpRoot("lr-hook-alone-");
  installFakeHook(dir, "ALONE");
  assert.equal(fs.readdirSync(dir).length, 1, "only local-rag-hook is present");
  assert.match(runHook("Stop", [dir]).stdout, /^ALONE /);
});

/**
 * Every `sh` on this host, so a parity check cannot pass for the accidental
 * reason that the developer's `/bin/sh` happens to be forgiving. Absent
 * shells are skipped rather than failed — which of them exist is a property
 * of the machine, not of this repository.
 */
function availableShells() {
  return ["/bin/sh", "/bin/dash", "/bin/bash", "/bin/zsh"].filter((s) => {
    try {
      fs.accessSync(s, fs.constants.X_OK);
      return true;
    } catch {
      return false;
    }
  });
}

test("LOCAL_RAG_BIN_DIR wins over PATH — 13 §2's order, for the hook path too", () => {
  // Asserted through the real rungs, with the test seam deliberately unset:
  // the seam is what every other case here uses, so nothing else in this file
  // would notice if the production first rung stopped being consulted.
  const override = mkTmpRoot("lr-hook-override-");
  const onPath = mkTmpRoot("lr-hook-onpath-");
  installFakeHook(override, "OVERRIDE");
  installFakeHook(onPath, "ON-PATH");
  const base = { CLAUDE_PLUGIN_ROOT: PLUGIN_ROOT, HOME: mkTmpRoot("lr-hook-emptyhome-") };
  const run = (env) =>
    spawnSync("/bin/sh", ["-c", hookCommand("Stop")], { input: "{}", encoding: "utf8", env });

  assert.match(run({ ...base, PATH: onPath }).stdout, /^ON-PATH /);
  assert.match(run({ ...base, PATH: onPath, LOCAL_RAG_BIN_DIR: override }).stdout, /^OVERRIDE /);
});

test("the notice survives a PATH with no coreutils on it — no external command", () => {
  // Found by a test whose PATH held only its own fixture directory: the
  // notice was printed with `cat`, so on a minimal PATH it vanished and
  // `SessionStart` fell silent — the one case the notice exists for. A hook
  // that must work under launchd's four-entry PATH cannot depend on anything
  // outside the shell itself.
  const empty = mkTmpRoot("lr-hook-empty-");
  const bare = mkTmpRoot("lr-hook-bare-path-");
  const r = spawnSync("/bin/sh", ["-c", hookCommand("SessionStart")], {
    input: "{}",
    encoding: "utf8",
    env: {
      PATH: bare,
      HOME: bare,
      CLAUDE_PLUGIN_ROOT: PLUGIN_ROOT,
      LOCAL_RAG_TEST_BIN_DIRS: empty,
    },
  });
  assert.equal(r.status, 0);
  assert.equal(r.stdout, fs.readFileSync(GOLDEN_FILE, "utf8"));
});

test("the test seam replaces the whole candidate list rather than extending it", () => {
  // The same property the JS launcher's own suite pins, for the same reason:
  // a seam that prepended would let the developer's real environment decide
  // an outcome, and every "it resolved" in this file would stop meaning "it
  // resolved where the test put it".
  const real = mkTmpRoot("lr-hook-real-");
  installFakeHook(real, "REAL-RUNG");
  const empty = mkTmpRoot("lr-hook-empty-");
  const r = spawnSync("/bin/sh", ["-c", hookCommand("SessionStart")], {
    input: "{}",
    encoding: "utf8",
    env: {
      PATH: real,
      HOME: real,
      CLAUDE_PLUGIN_ROOT: PLUGIN_ROOT,
      LOCAL_RAG_BIN_DIR: real,
      LOCAL_RAG_TEST_BIN_DIRS: empty,
    },
  });
  assert.doesNotMatch(r.stdout, /REAL-RUNG/, "neither PATH nor the override may leak past the seam");
  assert.equal(r.stdout, fs.readFileSync(GOLDEN_FILE, "utf8"));
});

test("--print-candidates agrees with candidateBinDirs(), entry for entry", () => {
  // T22-14 owns this as a card requirement; it is asserted here because it
  // already holds, and a parity that is only checked later is a parity that
  // drifts in between. The two implementations are held to the same list, not
  // to the same prose.
  const home = mkTmpRoot("lr-hook-home-");
  const nodeDir = path.dirname(process.execPath);

  // The trailing-slash shapes are a measured finding, not a hypothetical.
  // `command -v` concatenates the PATH entry with the name, so under dash a
  // `PATH` of `/usr/bin/` produced `/usr/bin//node` and the shell's list came
  // out one entry shorter than `path.dirname()`'s. bash normalises and hid
  // it; only running both shells surfaced it.
  for (const suffix of ["", "/", "//"]) {
    const env = {
      PATH: [nodeDir + suffix, "/usr/bin", "/bin"].join(":"),
      HOME: home,
      LOCAL_RAG_BIN_DIR: path.join(home, "override"),
      // D-124's rung, carried through the same trailing-slash shapes: it is the
      // second place a child path is formed, so it is the second place the two
      // implementations can disagree about `//bin` versus `/bin`.
      PNPM_HOME: path.join(home, "pnpm") + suffix,
    };
    const fromJs = candidateBinDirs({ env, platform: "linux", execPath: process.execPath });
    assert.ok(fromJs.length > 5, "a degenerate empty list would satisfy deepEqual trivially");
    for (const shell of availableShells()) {
      const r = spawnSync(shell, [RESOLVER, "--print-candidates"], { encoding: "utf8", env });
      assert.equal(r.status, 0, `${shell} exited ${r.status}: ${r.stderr}`);
      const fromShell = r.stdout.split("\n").filter((l) => l !== "");
      assert.deepEqual(fromShell, fromJs, `${shell} with PATH entry "${nodeDir}${suffix}"`);
    }
  }
});

test("the shell omits node's own directory when node is not on PATH — the one known divergence", () => {
  // Recorded rather than papered over. JS reads `process.execPath`, which it
  // always knows; a shell can only ask `command -v node`. Pinned so T22-14's
  // parity test cannot be surprised by it, and so that a future "fix" that
  // makes the shell guess a node directory shows up here as a change.
  const home = mkTmpRoot("lr-hook-nonode-");
  const noNode = mkTmpRoot("lr-hook-binless-");
  const env = { PATH: noNode, HOME: home };
  const shell = spawnSync(RESOLVER, ["--print-candidates"], { encoding: "utf8", env });
  const fromShell = shell.stdout.split("\n").filter((l) => l !== "");
  const fromJs = candidateBinDirs({ env, platform: "linux", execPath: process.execPath });
  const nodeDir = path.dirname(process.execPath);
  assert.ok(fromJs.includes(nodeDir), "JS derives node's directory from process.execPath");
  assert.ok(!fromShell.includes(nodeDir), "the shell cannot, and does not pretend to");
  // The divergence is exactly one entry, and the absolute well-known rungs —
  // the ones that matter in this very scenario — are unaffected.
  assert.deepEqual(fromJs.filter((d) => d !== nodeDir), fromShell);
});

test("the not-installed envelope matches what print_hook_output actually emits", () => {
  const raw = fs.readFileSync(GOLDEN_FILE, "utf8");
  assert.ok(raw.endsWith("\n"), "one trailing newline, as `line.push('\\n')` produces");
  assert.equal(raw.indexOf("\n"), raw.length - 1, "exactly one newline, at the end");

  const parsed = JSON.parse(raw);
  // Round-trip proves compactness: `serde_json::to_string` emits no spaces,
  // and `JSON.stringify` preserves the parsed key order, so any reformatting
  // of this file shows up here.
  assert.equal(JSON.stringify(parsed), raw.slice(0, -1));

  const outer = Object.keys(parsed);
  const inner = Object.keys(parsed.hookSpecificOutput);
  assert.deepEqual(outer, ["hookSpecificOutput"]);
  // ALPHABETICAL, not source order. `recall.rs` writes `hookEventName` first
  // in its `json!` literal, but `serde_json::Map` is a `BTreeMap` unless the
  // `preserve_order` feature is on — and it is not (asserted below). Writing
  // this golden in source order would produce valid JSON that is not the
  // bytes the binary emits.
  assert.deepEqual(inner, [...inner].sort());
  assert.deepEqual(inner, ["additionalContext", "hookEventName"]);
  assert.equal(parsed.hookSpecificOutput.hookEventName, "SessionStart");

  // The reason for the ordering, checked at its source rather than assumed:
  // `preserve_order` pulls in `indexmap`, so its absence from serde_json's
  // dependency list in the lockfile is what makes `BTreeMap` the map type.
  const lock = fs.readFileSync(path.join(REPO_ROOT, "Cargo.lock"), "utf8");
  const block = /\[\[package\]\]\nname = "serde_json"\n[\s\S]*?(?=\n\[\[package\]\])/.exec(lock);
  assert.ok(block, "serde_json must be in Cargo.lock");
  assert.ok(
    !block[0].includes("indexmap"),
    "serde_json gained `preserve_order`; this golden's key order is now wrong",
  );

  // Anchored to the producer's own field names, so a rename in `recall.rs`
  // cannot leave this file agreeing only with itself.
  const recallRs = fs.readFileSync(
    path.join(REPO_ROOT, "crates", "local-rag-hook", "src", "recall.rs"),
    "utf8",
  );
  for (const key of ["hookSpecificOutput", "hookEventName", "additionalContext"]) {
    assert.ok(recallRs.includes(`"${key}"`), `recall.rs must still emit ${key}`);
  }
});

test("the notice names what is missing and the one command that fixes it", () => {
  const text = JSON.parse(fs.readFileSync(GOLDEN_FILE, "utf8")).hookSpecificOutput.additionalContext;
  assert.match(text, /not installed/);
  assert.match(text, /npm i -g @13w\/memory/);
  assert.match(text, /LOCAL_RAG_BIN_DIR/, "the offline route must be stated, not only the online one");
  assert.doesNotMatch(text, /npx/, "ADR-0013 Decision 3: nothing runs npx");
});
