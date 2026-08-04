#!/usr/bin/env node
"use strict";

// Thin launcher entrypoint (spec 13 §1/§2). Resolves this host's platform
// package, execs the native `local-rag-proxy` in place, and forwards
// SIGINT/SIGTERM/exit-code 1:1 — no protocol-level work happens here (see
// `../src/lifecycle.js`'s doc comment for why `stdio: 'inherit'` is
// correct rather than a manual pipe/relay).

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

  const execPath = binaryPath(result.packageDir, process.platform, "local-rag-proxy");
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
