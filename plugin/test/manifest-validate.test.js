"use strict";

// Card requirement: "manifest/schema" — validated by the real `claude
// plugin validate --strict` CLI, not a reimplementation of its schema
// checks (hermetic: validate never touches `CLAUDE_CONFIG_DIR` state).

const { test } = require("node:test");
const assert = require("node:assert/strict");
const { spawnSync } = require("node:child_process");
const path = require("node:path");

const { claudeCliAvailable } = require("./helpers/claude-availability.js");

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const SKIP_REASON = "claude CLI not found on PATH";

function claudeValidate(target) {
  return spawnSync("claude", ["plugin", "validate", target, "--strict"], {
    encoding: "utf8",
  });
}

test(
  "plugin manifest (plugin/) validates cleanly under --strict",
  { skip: !claudeCliAvailable() && SKIP_REASON },
  () => {
    const result = claudeValidate(path.join(REPO_ROOT, "plugin"));
    assert.equal(result.status, 0, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);
    assert.match(result.stdout, /Validation passed/);
  },
);

test(
  "marketplace manifest (repo root) validates cleanly under --strict",
  { skip: !claudeCliAvailable() && SKIP_REASON },
  () => {
    const result = claudeValidate(REPO_ROOT);
    assert.equal(result.status, 0, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);
    assert.match(result.stdout, /Validation passed/);
  },
);
