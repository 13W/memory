#!/usr/bin/env node
"use strict";

// Scriptable stand-in for the real `npx` CLI, used only by
// `mcp-launcher-tiers.test.js`'s tier-3 tests. Records the exact argv it
// was invoked with (one JSON line, so a test can assert tier 3 was reached
// with the right arguments) then behaves like `fake-binary.js`: prints a
// READY line and stays alive for signal-forwarding to make sense, so a
// tier-3 run looks like a genuinely working server from the launcher's
// point of view, not just a stub that immediately exits.

process.stdout.write(`NPX_ARGS ${JSON.stringify(process.argv.slice(2))}\n`);
process.stdout.write(`READY pid=${process.pid}\n`);

function onSignal(signal) {
  process.stdout.write(`EXITING ${signal}\n`);
  process.exit(0);
}

process.on("SIGINT", () => onSignal("SIGINT"));
process.on("SIGTERM", () => onSignal("SIGTERM"));

setInterval(() => {}, 0x7fffffff);
