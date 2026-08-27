"use strict";

// Card requirement: "exact hooks list" — spec 11 §3.1 `[FIXED]` names
// exactly these seven events. Pure JSON checks only; the real
// `claude plugin details`-backed confirmation lives in
// `install-uninstall.test.js` (see that file's own doc comment for why —
// concurrent `claude` CLI invocations against the same source repo raced
// when split across files). What the commands *do* when executed is
// `hook-resolution.test.js`'s subject; this file is about their shape.

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

function commandFor(hooks, event) {
  const entries = hooks.hooks[event];
  assert.equal(entries.length, 1, `${event}: expected exactly one matcher entry`);
  assert.equal(entries[0].hooks[0].type, "command");
  return entries[0].hooks[0].command;
}

test("every event runs the shipped resolver and guarantees exit 0", () => {
  const hooks = loadHooksJson();
  for (const event of SPEC_11_3_1_EVENTS) {
    const command = commandFor(hooks, event);
    assert.match(
      command,
      /"\$\{CLAUDE_PLUGIN_ROOT\}"\/bin\/local-rag-resolve-hook\.sh spool-write/,
      `${event}'s command must invoke the shipped resolver with spool-write`,
    );
    // `|| true` is what makes 11 §3.1's `[FIXED]` "always exit 0" true, and
    // it covers the whole command rather than the binary alone — the
    // resolver deliberately exits 127 when it finds nothing. Keeping this
    // assertion is also what rules out the rejected `[ $? -eq 127 ]` form of
    // the SessionStart line, which would not have ended here.
    assert.match(command, /\|\| true$/, `${event}'s command must guarantee exit 0 even if every path fails`);
  }
});

test("nothing runs npx, and nothing looks in the retired plugin-data cache", () => {
  // ADR-0013 Decision 3: "Nothing consults `node_modules`, and nothing runs
  // `npx`." Until T22-13 every one of these seven lines did both — that was
  // D-103, and this is the assertion that keeps it closed. `_DATA` is the
  // plugin's persistent-data directory; the resolver ships with the plugin
  // itself, so it lives under `_ROOT` (CONTRIBUTING.md audits the difference).
  const hooks = loadHooksJson();
  for (const event of SPEC_11_3_1_EVENTS) {
    const command = commandFor(hooks, event);
    assert.doesNotMatch(command, /npx/, `${event}'s command must not run npx`);
    assert.doesNotMatch(command, /node_modules/, `${event}'s command must not consult node_modules`);
    assert.doesNotMatch(command, /CLAUDE_PLUGIN_DATA/, `${event}'s command must not use the retired cache`);
  }
});

test("six commands are byte-identical; SessionStart differs by exactly one variable", () => {
  // "Six stay silent, the seventh speaks" (11 §3.2) is meant to be a
  // structural property, not seven independently-maintained strings. The
  // decision belongs to the resolver, which is the only party that knows
  // whether the binary was found; all `hooks.json` does is hand SessionStart
  // the file to print.
  const hooks = loadHooksJson();
  const silent = SPEC_11_3_1_EVENTS.filter((e) => e !== "SessionStart").map((e) => commandFor(hooks, e));
  assert.equal(new Set(silent).size, 1, "the six non-SessionStart commands must be one string");
  const prefix = 'LOCAL_RAG_NOT_INSTALLED_JSON="${CLAUDE_PLUGIN_ROOT}"/hooks/not-installed.json ';
  assert.equal(commandFor(hooks, "SessionStart"), prefix + silent[0]);
});

test("every file the seven commands name is actually shipped and runnable", () => {
  // A command line that references a path is only as good as the path. Both
  // of these are new files in the plugin payload, and a missing executable
  // bit on the resolver would fail open into permanent silence.
  const resolver = path.join(REPO_ROOT, "plugin", "bin", "local-rag-resolve-hook.sh");
  const golden = path.join(REPO_ROOT, "plugin", "hooks", "not-installed.json");
  assert.doesNotThrow(() => fs.accessSync(resolver, fs.constants.X_OK), `${resolver} must be executable`);
  assert.doesNotThrow(() => fs.accessSync(golden, fs.constants.R_OK), `${golden} must be readable`);
});
