#!/usr/bin/env node
"use strict";

// Thin launcher entrypoint (spec 13 §1/§2). Resolves this host's platform
// package, execs the native `local-rag-proxy` in place, and forwards
// SIGINT/SIGTERM/exit-code 1:1 — no protocol-level work happens here (see
// `../src/lifecycle.js`'s doc comment for why `stdio: 'inherit'` is
// correct rather than a manual pipe/relay).
//
// `refreshCache` (T19-03): every successful resolution here also refreshes
// `${CLAUDE_PLUGIN_DATA}/bin/local-rag-proxy`, the same cache the hook
// launcher already keeps for `local-rag-hook`. This file is reached both
// when `plugin/bin/local-rag-mcp-launcher.js`'s tier 1 `require()`s it
// (a locally installed `@13w/memory`) and when tier 3's `npx` fallback
// invokes it — either path populates the cache tier 2 reads, with no
// separate write-side code needed in the plugin launcher itself.

const { constants } = require("node:os");

const { resolvePlatformPackage, binaryPath } = require("../src/resolve.js");
const { formatMissingPlatformError } = require("../src/errors.js");
const { runAndForwardSignals } = require("../src/lifecycle.js");
const { refreshCache } = require("../src/binary-cache.js");

function main() {
  const result = resolvePlatformPackage(__filename);
  if (!result.ok) {
    console.error(formatMissingPlatformError(result));
    process.exit(1);
    return;
  }

  const execPath = binaryPath(result.packageDir, process.platform, "local-rag-proxy");
  refreshCache(execPath, "local-rag-proxy");
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
