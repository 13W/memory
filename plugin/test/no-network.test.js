"use strict";

// T22-14's acceptance: "the plugin never downloads anything" stops being a
// comment and becomes a test. ADR-0013 Decision 3 states it — "Nothing consults
// `node_modules`, and nothing runs `npx`" — and spec 13 §2 `[SPEC]` makes it a
// rule the plugin must obey, not an aspiration.
//
// TWO LAYERS, AND THE SECOND IS NOT DECORATION. A poisoned `PATH` proves no
// external downloader is invoked. It would say nothing about an `https.get`
// inside the launcher itself, which is precisely the way to download without
// calling anything — so the shipped files are also read and checked. Either
// layer alone would pass a plugin that had the other kind of network access.
//
// The `PATH` trap is the inversion of `helpers/fake-npx.js`, which existed to
// prove `npx` *was* reached on the launcher's old third tier. That tier is gone
// (T22-12), the helper went with it (T22-14), and the same technique now proves
// the opposite.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawnSync, spawn } = require("node:child_process");

const { mkTmpRoot } = require("./helpers/tmp.js");

const REPO_ROOT = path.resolve(__dirname, "..", "..");
const PLUGIN_ROOT = path.join(REPO_ROOT, "plugin");
const LAUNCHER_FILE = path.join(PLUGIN_ROOT, "bin", "local-rag-mcp-launcher.js");
const RESOLVER_FILE = path.join(PLUGIN_ROOT, "bin", "local-rag-resolve-hook.sh");

const SPEC_11_3_1_EVENTS = [
  "SessionStart",
  "UserPromptSubmit",
  "PostToolUse",
  "PostToolUseFailure",
  "Stop",
  "SubagentStop",
  "SessionEnd",
];

// Everything that could fetch a byte on a developer's or a user's machine. `npm`
// and its two rivals are here as well as `npx`: a launcher that shelled out to
// `npm exec` or `pnpm dlx` would be just as much a download.
const DOWNLOADERS = ["npx", "npm", "pnpm", "yarn", "bunx", "curl", "wget", "git"];

/**
 * A directory of executables that record being run instead of doing anything,
 * and the marker file they record into. Nothing here fails: a shim that exited
 * non-zero would let a caller "handle" the failure and move on, and the point
 * is to catch the call, not the outcome.
 */
function armTrap(prefix) {
  const dir = mkTmpRoot(prefix);
  const marker = path.join(dir, "..", `${path.basename(dir)}.calls`);
  for (const name of DOWNLOADERS) {
    const p = path.join(dir, name);
    fs.writeFileSync(p, `#!/bin/sh\nprintf '%s %s\\n' ${name} "$*" >> ${JSON.stringify(marker)}\nexit 0\n`);
    fs.chmodSync(p, 0o755);
  }
  return {
    dir,
    marker,
    /** @returns {string[]} one line per call, empty when nothing was invoked. */
    calls() {
      if (!fs.existsSync(marker)) return [];
      return fs.readFileSync(marker, "utf8").split("\n").filter((l) => l !== "");
    },
  };
}

/** A directory holding executable stand-ins for the named binaries. */
function binDirWith(prefix, names) {
  const dir = mkTmpRoot(prefix);
  for (const name of names) {
    const p = path.join(dir, name);
    fs.writeFileSync(p, "#!/bin/sh\nexit 0\n");
    fs.chmodSync(p, 0o755);
  }
  return dir;
}

/**
 * `PATH` is the trap and nothing else — not the trap prepended to the real one.
 * Prepending would leave the developer's own `npx` reachable by absolute path
 * and, worse, let a passing run mean "the real npx was faster to find".
 */
function trappedEnv(trap, extra = {}) {
  return {
    PATH: trap.dir,
    HOME: trap.dir,
    CLAUDE_PLUGIN_ROOT: PLUGIN_ROOT,
    ...extra,
  };
}

test("the trap is armed — a downloader on this PATH really does record itself", () => {
  // Without this, every "the marker is absent" assertion below would also pass
  // for a trap that was never executable, never on PATH, or writing somewhere
  // nobody reads.
  const trap = armTrap("lr-nonet-armed-");
  for (const name of DOWNLOADERS) {
    const r = spawnSync("/bin/sh", ["-c", `${name} --version`], { env: trappedEnv(trap) });
    assert.equal(r.status, 0, `${name} shim must be runnable from the trapped PATH`);
  }
  assert.deepEqual(
    trap.calls().map((l) => l.split(" ")[0]),
    DOWNLOADERS,
    "each shim must record its own name",
  );
});

test("the MCP launcher runs no downloader, whether or not it resolves a server", async () => {
  const trap = armTrap("lr-nonet-mcp-");
  const installed = binDirWith("lr-nonet-mcpbin-", ["local-rag-proxy", "local-rag"]);

  for (const [label, testDirs] of [
    ["resolved", installed],
    ["not installed", path.join(trap.dir, "nothing-here")],
  ]) {
    const launcher = spawn(process.execPath, [LAUNCHER_FILE], {
      stdio: ["ignore", "pipe", "pipe"],
      env: trappedEnv(trap, { LOCAL_RAG_TEST_BIN_DIRS: testDirs }),
    });
    await new Promise((resolve) => launcher.on("exit", resolve));
    assert.deepEqual(trap.calls(), [], `${label}: the launcher invoked a downloader`);
  }
});

test("no hooks.json command runs a downloader, whether or not it resolves the hook", () => {
  const hooks = JSON.parse(fs.readFileSync(path.join(PLUGIN_ROOT, "hooks", "hooks.json"), "utf8"));
  const trap = armTrap("lr-nonet-hook-");
  const installed = binDirWith("lr-nonet-hookbin-", ["local-rag-hook"]);

  for (const [label, testDirs] of [
    ["resolved", installed],
    ["not installed", path.join(trap.dir, "nothing-here")],
  ]) {
    for (const event of SPEC_11_3_1_EVENTS) {
      const command = hooks.hooks[event][0].hooks[0].command;
      const r = spawnSync("/bin/sh", ["-c", command], {
        input: "{}",
        encoding: "utf8",
        env: trappedEnv(trap, { LOCAL_RAG_TEST_BIN_DIRS: testDirs }),
      });
      // 11 §3.1 `[FIXED]` still holds under a hostile PATH — worth asserting
      // here and not only in `hook-resolution.test.js`, because a command that
      // died early would be a cheap way to invoke nothing.
      assert.equal(r.status, 0, `${label}/${event}: must still exit 0`);
      assert.deepEqual(trap.calls(), [], `${label}/${event}: the hook invoked a downloader`);
    }
  }
});

test("neither channel recreates the retired ${CLAUDE_PLUGIN_DATA} cache", () => {
  // ADR-0013 Decision 3 removed that tier; nothing writes there today. This is
  // the guard against its return — a cache keyed by path is exactly what made
  // the old design point a stale proxy at a stale daemon (D-103).
  const hooks = JSON.parse(fs.readFileSync(path.join(PLUGIN_ROOT, "hooks", "hooks.json"), "utf8"));
  const trap = armTrap("lr-nonet-data-");
  const pluginData = mkTmpRoot("lr-nonet-plugindata-");
  const installed = binDirWith("lr-nonet-databin-", ["local-rag-hook", "local-rag-proxy", "local-rag"]);
  // A NORMAL `PATH` here, deliberately, unlike every other test in this file.
  // Under the trap's `PATH` this assertion was blind: a mutation that wrote to
  // the cache with `mkdir` passed, because `mkdir` was not there to be found.
  // Downloaders are the other tests' subject; this one is about writes.
  const env = {
    PATH: "/usr/bin:/bin",
    HOME: trap.dir,
    CLAUDE_PLUGIN_ROOT: PLUGIN_ROOT,
    LOCAL_RAG_TEST_BIN_DIRS: installed,
    CLAUDE_PLUGIN_DATA: pluginData,
  };

  spawnSync(process.execPath, [LAUNCHER_FILE], { stdio: "ignore", env });
  for (const event of SPEC_11_3_1_EVENTS) {
    spawnSync("/bin/sh", ["-c", hooks.hooks[event][0].hooks[0].command], { input: "{}", env });
  }
  assert.deepEqual(fs.readdirSync(pluginData), [], "nothing may be written under CLAUDE_PLUGIN_DATA");
});

/**
 * Source with comments removed, so a file may keep explaining what it avoids.
 *
 * LINE COMMENTS FIRST, THEN BLOCK COMMENTS, and the order is the whole
 * correctness argument. The launcher's header contains the line
 * `// ... under npm/memory/src/*. It resolves an` — a `/*` inside a line
 * comment. Stripping blocks first made that the opening of a block that ran to
 * the next `*&#47;` forty lines later, swallowing every `require()` in the
 * file; the check below then vouched for code it had never looked at. Found by
 * a mutation that added `require("node:https")` and stayed green, not by
 * reading this function.
 */
function strippedSource(file) {
  const withoutLineComments = fs
    .readFileSync(file, "utf8")
    .split("\n")
    .filter((l) => !l.trimStart().startsWith(file.endsWith(".sh") ? "#" : "//"))
    .join("\n");
  return file.endsWith(".sh")
    ? withoutLineComments
    : withoutLineComments.replace(/\/\*[\s\S]*?\*\//g, "");
}

// Each shipped file, with a word its own prose uses for something it
// deliberately does NOT do. That word is what proves the stripping works: it
// must be present in the raw file and absent from the stripped one.
const SHIPPED = [
  {
    file: LAUNCHER_FILE,
    proseOnly: /\bnpx\b/,
    // Landmarks: code that must survive stripping. Without them a stripper
    // that ate half the file would make every `doesNotMatch` below true for
    // the worst possible reason. This is what caught the block-comment bug.
    landmarks: [/require\("node:fs"\)/, /function candidateBinDirs/, /function resolveBinary/],
  },
  // The resolver's header never mentions `npx` — it argues about external
  // commands generally, naming `awk` among the ones it does not use.
  {
    file: RESOLVER_FILE,
    proseOnly: /\bawk\b/,
    landmarks: [/exec "\$_found"/, /_emit_candidates\(\) \{/, /command -v node/],
  },
];

test("neither shipped file can reach the network without an external command", () => {
  // The layer the PATH trap cannot provide. `release-urls.test.js` already
  // checks `src/release.js` this way; the same idiom, for the two files the
  // plugin actually ships.
  for (const { file } of SHIPPED) {
    const code = strippedSource(file);
    const name = path.basename(file);
    assert.doesNotMatch(code, /require\(["']node:https?["']\)/, `${name} must not require an HTTP module`);
    assert.doesNotMatch(code, /\bfetch\s*\(/, `${name} must not call fetch`);
    assert.doesNotMatch(code, /\bnpx\b/, `${name} must not name npx`);
    assert.doesNotMatch(code, /node_modules/, `${name} must not consult node_modules`);
    assert.doesNotMatch(code, /\bcurl\b|\bwget\b/, `${name} must not shell out to a downloader`);
  }
});

test("the comment-stripping is real, not a function that returns nothing", () => {
  // Without this the test above passes for a stripper that hands back an empty
  // string — the classic way a source check becomes decorative.
  for (const { file, proseOnly, landmarks } of SHIPPED) {
    const name = path.basename(file);
    const raw = fs.readFileSync(file, "utf8");
    const code = strippedSource(file);
    assert.match(raw, proseOnly, `${name}: this file's prose should still contain the marker word`);
    assert.doesNotMatch(code, proseOnly, `${name}: stripping must remove the prose`);
    for (const landmark of landmarks) {
      assert.match(code, landmark, `${name}: stripping ate code it should have kept`);
    }
  }
});
