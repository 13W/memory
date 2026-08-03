"use strict";

// Deadline-polling helpers for real-subprocess tests — the same idiom
// `crates/local-rag-proxy/tests/subprocess.rs` uses (`wait_for_exit`,
// `pid_exists`) instead of a single blocking wait or a fixed sleep.

/**
 * Poll `check()` every `intervalMs` until it returns a truthy value or
 * `timeoutMs` elapses, then throw.
 *
 * @template T
 * @param {() => T | Promise<T>} check
 * @param {{timeoutMs?: number, intervalMs?: number, description?: string}} [opts]
 * @returns {Promise<T>}
 */
async function waitUntil(check, opts = {}) {
  const timeoutMs = opts.timeoutMs ?? 5000;
  const intervalMs = opts.intervalMs ?? 20;
  const description = opts.description ?? "condition";
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const result = await check();
    if (result) {
      return result;
    }
    if (Date.now() >= deadline) {
      throw new Error(`timed out after ${timeoutMs}ms waiting for: ${description}`);
    }
    await new Promise((resolve) => setTimeout(resolve, intervalMs));
  }
}

/**
 * Portable liveness probe via `kill(pid, 0)` — no signal is actually sent;
 * a thrown `ESRCH` means the pid is gone, `EPERM` means it exists but is
 * owned by someone else (still alive).
 *
 * @param {number} pid
 * @returns {boolean}
 */
function pidIsAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err && err.code === "EPERM";
  }
}

/**
 * Collects stdout line-by-line and resolves the returned promise the
 * moment a line matching `predicate` appears, without discarding any of
 * the previously-seen lines (`lines` is filled as they arrive so a caller
 * can also assert on ordering afterward).
 *
 * @param {import('node:child_process').ChildProcessWithoutNullStreams} child
 * @param {(line: string) => boolean} predicate
 * @param {{timeoutMs?: number}} [opts]
 * @returns {Promise<{line: string, lines: string[]}>}
 */
function waitForStdoutLine(child, predicate, opts = {}) {
  const timeoutMs = opts.timeoutMs ?? 5000;
  const lines = [];
  let buffer = "";
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error(`timed out after ${timeoutMs}ms waiting for a matching stdout line`));
    }, timeoutMs);

    function onData(chunk) {
      buffer += chunk.toString("utf8");
      let idx;
      // eslint-disable-next-line no-cond-assign
      while ((idx = buffer.indexOf("\n")) !== -1) {
        const line = buffer.slice(0, idx);
        buffer = buffer.slice(idx + 1);
        lines.push(line);
        if (predicate(line)) {
          cleanup();
          resolve({ line, lines });
          return;
        }
      }
    }

    function cleanup() {
      clearTimeout(timer);
      child.stdout.off("data", onData);
    }

    child.stdout.on("data", onData);
  });
}

module.exports = { waitUntil, pidIsAlive, waitForStdoutLine };
