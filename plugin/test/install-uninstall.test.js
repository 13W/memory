"use strict";

// Card requirement: "install/uninstall fixture" — a real
// `marketplace add` → `install` → `list` → `uninstall` round trip through
// the real `claude` CLI, isolated from the developer's real `~/.claude` by
// `CLAUDE_CONFIG_DIR` (a genuinely respected env var — this very
// development session runs under one), the same isolation idiom
// `LOCAL_RAG_HOME` already gives every Rust test in this repository.
//
// All real-`claude`-CLI tests in this suite live in this one file
// (including the "exact hooks list" card requirement's `claude plugin
// details` check, folded into the round trip below) deliberately: Node's
// test runner parallelizes across *files* by default, and concurrent
// `claude plugin marketplace add`/`install` invocations against the same
// source repo path raced and flaked when split across files (observed
// empirically — `hooks-list.test.js`'s own `details` check failed only
// when run alongside this file, never in isolation). Tests within one file
// run sequentially by default, which is what these need.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const { claudeCliAvailable } = require("./helpers/claude-availability.js");

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const SKIP_REASON = "claude CLI not found on PATH";

const SPEC_11_3_1_EVENTS = [
  "SessionStart",
  "UserPromptSubmit",
  "PostToolUse",
  "PostToolUseFailure",
  "Stop",
  "SubagentStop",
  "SessionEnd",
];

function runClaude(args, configDir) {
  return spawnSync("claude", args, {
    encoding: "utf8",
    env: { ...process.env, CLAUDE_CONFIG_DIR: configDir },
  });
}

test(
  "marketplace add -> install -> list -> uninstall round-trips cleanly under an isolated CLAUDE_CONFIG_DIR",
  { skip: !claudeCliAvailable() && SKIP_REASON },
  () => {
    const configDir = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cfg-"));
    try {
      let r = runClaude(["plugin", "marketplace", "add", REPO_ROOT], configDir);
      assert.equal(r.status, 0, `marketplace add: ${r.stdout}\n${r.stderr}`);

      r = runClaude(["plugin", "install", "memory@memory", "-s", "local"], configDir);
      assert.equal(r.status, 0, `install: ${r.stdout}\n${r.stderr}`);

      r = runClaude(["plugin", "list", "--json"], configDir);
      assert.equal(r.status, 0, `list: ${r.stdout}\n${r.stderr}`);
      const installed = JSON.parse(r.stdout);
      assert.equal(installed.length, 1);
      assert.equal(installed[0].id, "memory@memory");
      assert.equal(installed[0].enabled, true);
      assert.equal(installed[0].scope, "local");

      // Card requirement "exact hooks list", verified against the real
      // installed plugin (not just a JSON-file parse) — same install
      // session, no second marketplace add/install round trip needed.
      r = runClaude(["plugin", "details", "memory@memory"], configDir);
      assert.equal(r.status, 0, `details: ${r.stdout}\n${r.stderr}`);
      assert.match(r.stdout, /Hooks \(7\)/);
      for (const event of SPEC_11_3_1_EVENTS) {
        assert.match(r.stdout, new RegExp(event), `expected ${event} in claude plugin details output`);
      }

      // T19-04: the plugin skill (`plugin/skills/memory-first-workflow/`)
      // is auto-discovered from its default-location directory — no
      // `plugin.json` entry names it, so this is the only real proof it
      // was actually picked up on install, not just present on disk.
      assert.match(r.stdout, /Skills \(1\)/);
      assert.match(r.stdout, /memory-first-workflow/);

      r = runClaude(["plugin", "uninstall", "memory@memory", "-s", "local", "-y"], configDir);
      assert.equal(r.status, 0, `uninstall: ${r.stdout}\n${r.stderr}`);

      r = runClaude(["plugin", "list", "--json"], configDir);
      assert.equal(r.status, 0, `list (post-uninstall): ${r.stdout}\n${r.stderr}`);
      assert.deepEqual(JSON.parse(r.stdout), []);
    } finally {
      fs.rmSync(configDir, { recursive: true, force: true });
    }
  },
);

test(
  "the round trip never writes into the repository itself",
  { skip: !claudeCliAvailable() && SKIP_REASON },
  () => {
    const before = spawnSync("git", ["status", "--porcelain"], { cwd: REPO_ROOT, encoding: "utf8" }).stdout;
    const configDir = fs.mkdtempSync(path.join(os.tmpdir(), "lr-plugin-cfg-nowrite-"));
    try {
      runClaude(["plugin", "marketplace", "add", REPO_ROOT], configDir);
      runClaude(["plugin", "install", "memory@memory", "-s", "local"], configDir);
      runClaude(["plugin", "details", "memory@memory"], configDir);
      runClaude(["plugin", "uninstall", "memory@memory", "-s", "local", "-y"], configDir);
    } finally {
      fs.rmSync(configDir, { recursive: true, force: true });
    }
    const after = spawnSync("git", ["status", "--porcelain"], { cwd: REPO_ROOT, encoding: "utf8" }).stdout;
    assert.equal(after, before, "the repository's own git status must be unchanged by the plugin lifecycle");
  },
);
