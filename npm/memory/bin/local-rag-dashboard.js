#!/usr/bin/env node
"use strict";

// Thin launcher entrypoint for the TUI dashboard (ADR-0008, spec 11 §7). Resolves this host's
// platform package, execs the native `local-rag-tui` in place, and forwards SIGINT/SIGTERM/exit
// code 1:1 — no protocol-level work happens here, mirroring `local-rag-mcp.js`. Unlike the MCP
// proxy, `stdio: 'inherit'` is not just convenient here but load-bearing: the dashboard is a
// real, full-screen terminal application — `crossterm`'s raw-mode/alternate-screen ioctls need
// the child attached to the real TTY this process itself inherited, not a pipe (a pipe has no
// terminal size/raw-mode semantics at all). See `../src/lifecycle.js`'s own doc comment for the
// signal-forwarding contract this shares with the MCP launcher.

const { constants } = require("node:os");

const { resolvePlatformPackage, binaryPath } = require("../src/resolve.js");
const { formatMissingPlatformError } = require("../src/errors.js");
const { runAndForwardSignals } = require("../src/lifecycle.js");

function main() {
  const result = resolvePlatformPackage(__filename);
  if (!result.ok) {
    console.error(formatMissingPlatformError(result));
    process.exit(1);
    return;
  }

  const execPath = binaryPath(result.packageDir, process.platform, "local-rag-tui");
  const args = process.argv.slice(2);

  runAndForwardSignals(execPath, args)
    .then(({ code, signal }) => {
      if (signal) {
        const signalNumber = constants.signals[signal];
        console.error(`local-rag: the native binary was terminated by ${signal}`);
        process.exit(128 + (signalNumber ?? 1));
        return;
      }
      process.exit(code ?? 1);
    })
    .catch((err) => {
      console.error(`local-rag: could not run the native binary: ${err.message}`);
      process.exit(1);
    });
}

main();
