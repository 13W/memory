#!/usr/bin/env node
"use strict";

// Scriptable stand-in for `local-rag-proxy`, used only by this launcher's
// own subprocess-tier tests. Prints a single READY line carrying its own
// pid as soon as it starts (the test's signal to send SIGINT/SIGTERM), then
// mirrors `local-rag-proxy`'s real behavior by default: handle SIGINT and
// SIGTERM gracefully and exit 0. `--ignore-first-signal` makes it swallow
// exactly one signal before behaving normally, for tests that need to prove
// a *redundant* forwarded signal (terminal Ctrl-C broadcast + explicit
// forward landing on the same process) is harmless, not double-handled.

const ignoreFirst = process.argv.includes("--ignore-first-signal");
let ignoredOnce = false;

process.stdout.write(`READY pid=${process.pid}\n`);

function onSignal(signal) {
  if (ignoreFirst && !ignoredOnce) {
    ignoredOnce = true;
    process.stdout.write(`IGNORED ${signal}\n`);
    return;
  }
  process.stdout.write(`EXITING ${signal}\n`);
  process.exit(0);
}

process.on("SIGINT", () => onSignal("SIGINT"));
process.on("SIGTERM", () => onSignal("SIGTERM"));

// Keep the event loop alive indefinitely until a signal arrives.
setInterval(() => {}, 0x7fffffff);
