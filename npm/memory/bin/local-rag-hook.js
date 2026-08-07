#!/usr/bin/env node
"use strict";

// Thin hook launcher entrypoint (spec 11 §3.1, T17-02). Unlike
// `local-rag-mcp.js`, this MUST ALWAYS exit 0 — hooks are fail-open by
// `[FIXED]` contract (spec 11 §3.1), and that contract covers the whole
// command a `hooks.json` entry invokes, not just the native binary once
// it's actually running. A missing/unresolvable platform package is a
// loud diagnostic on stderr here, never a fatal exit — the opposite of
// the MCP launcher, where the same condition is correctly fatal.

const { spawnSync } = require("node:child_process");

const { resolvePlatformPackage, binaryPath } = require("../src/resolve.js");
const { formatMissingPlatformError } = require("../src/errors.js");
const { refreshCache } = require("../src/binary-cache.js");

function main() {
  const result = resolvePlatformPackage(__filename);
  if (!result.ok) {
    console.error(formatMissingPlatformError(result));
    process.exit(0);
    return;
  }

  const execPath = binaryPath(result.packageDir, process.platform, "local-rag-hook");
  refreshCache(execPath, "local-rag-hook");

  const child = spawnSync(execPath, process.argv.slice(2), { stdio: "inherit" });
  if (child.error) {
    console.error(`local-rag: could not run the native hook binary: ${child.error.message}`);
    process.exit(0);
    return;
  }
  // The native binary's own `spool-write` path is already unconditionally
  // exit-0 (fail-open by its own [FIXED] contract) — passed through
  // faithfully rather than hardcoded, so `local-rag-hook version`
  // (diagnostic subcommand) still reports its real exit code, and a
  // process killed by a signal (`child.status === null`) still falls back
  // to 0 here rather than propagating `null`.
  process.exit(child.status ?? 0);
}

main();
