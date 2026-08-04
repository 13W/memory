"use strict";

const { spawn } = require("node:child_process");

const FORWARDED_SIGNALS = Object.freeze(["SIGINT", "SIGTERM"]);

/**
 * Run `execPath` as an attached (never `detached`) stdio-inherited child,
 * forwarding `SIGINT`/`SIGTERM` to it 1:1, and resolve once the child
 * itself exits — never before. This is the inverse of
 * `crates/local-rag-proxy/src/connect.rs::spawn_detached_daemon`, which
 * deliberately *isolates* a spawned daemon from the proxy's own signals
 * (`process_group(0)` + `Stdio::null()`); here the child must live and die
 * with this process, so no `detached`/process-group split is used at all.
 *
 * A real terminal Ctrl-C is broadcast by the OS to the whole foreground
 * process group — both this process and the (attached, same-group) child
 * receive `SIGINT` directly and simultaneously, with no forwarding
 * required for that path. This function still forwards every signal it
 * catches regardless: a forward that lands on an already-signaled or
 * already-exited child is a harmless, idempotent no-op (`child.kill`
 * swallows `ESRCH`-shaped failures), and `local-rag-proxy`'s own shutdown
 * listener only ever runs once no matter how many times a signal arrives.
 * The `exit` event — not the signal handler — is the single source of
 * truth for this function's own outcome.
 *
 * @param {string} execPath
 * @param {string[]} args
 * @param {{
 *   spawnFn?: typeof spawn,
 *   signalSource?: NodeJS.EventEmitter,
 *   signals?: string[],
 *   stdio?: import('node:child_process').StdioOptions,
 * }} [deps] - injected only by fast unit tests (a fake `EventEmitter` child
 *   plus a fake signal-source `EventEmitter` standing in for `process`);
 *   production omits all of these and gets the real `child_process.spawn`
 *   and the real `process`.
 * @returns {Promise<{code: number|null, signal: string|null}>}
 *   mirrors Node's own `child.on('exit', (code, signal) => ...)` shape
 *   verbatim; deciding what *this* process's own exit looks like is left to
 *   the caller, keeping this module callable from a test without ever
 *   invoking `process.exit()`.
 */
function runAndForwardSignals(execPath, args, deps = {}) {
  const spawnFn = deps.spawnFn ?? spawn;
  const signalSource = deps.signalSource ?? process;
  const signals = deps.signals ?? FORWARDED_SIGNALS;
  const stdio = deps.stdio ?? "inherit";

  return new Promise((resolve, reject) => {
    const child = spawnFn(execPath, args, { stdio });

    let shuttingDown = false;
    const handlers = new Map();

    function cleanup() {
      for (const [signal, handler] of handlers) {
        signalSource.off(signal, handler);
      }
      handlers.clear();
    }

    for (const signal of signals) {
      const handler = () => {
        if (shuttingDown) {
          return;
        }
        shuttingDown = true;
        try {
          child.kill(signal);
        } catch {
          // The child may already be gone (ESRCH-shaped failure) — the
          // `exit` handler below is the only place that decides what this
          // process does next, regardless of whether this kill landed.
        }
      };
      handlers.set(signal, handler);
      signalSource.on(signal, handler);
    }

    child.on("error", (err) => {
      cleanup();
      reject(err);
    });

    child.on("exit", (code, signal) => {
      cleanup();
      resolve({ code, signal });
    });
  });
}

module.exports = { runAndForwardSignals, FORWARDED_SIGNALS };
