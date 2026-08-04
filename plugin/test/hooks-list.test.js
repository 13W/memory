"use strict";

// Card requirement: "exact hooks list" — spec 11 §3.1 `[FIXED]` names
// exactly these seven events. Pure JSON checks only; the real
// `claude plugin details`-backed confirmation lives in
// `install-uninstall.test.js` (see that file's own doc comment for why —
// concurrent `claude` CLI invocations against the same source repo raced
// when split across files).

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const REPO_ROOT = path.resolve(__dirname, "..", "..");

const SPEC_11_3_1_EVENTS = [
  "SessionStart",
  "UserPromptSubmit",
  "PostToolUse",
  "PostToolUseFailure",
  "Stop",
  "SubagentStop",
  "SessionEnd",
];

function loadHooksJson() {
  return JSON.parse(fs.readFileSync(path.join(REPO_ROOT, "plugin", "hooks", "hooks.json"), "utf8"));
}

test("hooks.json declares exactly the seven spec 11 §3.1 events, no more, no fewer", () => {
  const hooks = loadHooksJson();
  assert.deepEqual(Object.keys(hooks.hooks).sort(), [...SPEC_11_3_1_EVENTS].sort());
});

test("every event uses the same fail-open fast/slow-path command, guaranteeing exit 0", () => {
  const hooks = loadHooksJson();
  for (const event of SPEC_11_3_1_EVENTS) {
    const entries = hooks.hooks[event];
    assert.equal(entries.length, 1, `${event}: expected exactly one matcher entry`);
    const command = entries[0].hooks[0].command;
    assert.match(command, /local-rag-hook spool-write/, `${event}'s command must invoke spool-write`);
    assert.match(command, /\|\| npx /, `${event}'s command must have an npx fallback for a cold cache`);
    assert.match(command, /\|\| true$/, `${event}'s command must guarantee exit 0 even if every path fails`);
    assert.equal(entries[0].hooks[0].type, "command");
  }
});
