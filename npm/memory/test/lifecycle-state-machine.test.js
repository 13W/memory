"use strict";

const { test } = require("node:test");
const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");

const { runAndForwardSignals } = require("../src/lifecycle.js");

/** A fake child_process.ChildProcess: an EventEmitter with a spy `kill()`. */
function makeFakeChild() {
  const child = new EventEmitter();
  child.killCalls = [];
  child.kill = (signal) => {
    child.killCalls.push(signal);
    return true;
  };
  return child;
}

function fakeDeps(child, signalSource) {
  return {
    spawnFn: () => child,
    signalSource,
  };
}

test("forwards a received SIGTERM to the child exactly once", async () => {
  const child = makeFakeChild();
  const signalSource = new EventEmitter();
  const promise = runAndForwardSignals("/bin/fake", [], fakeDeps(child, signalSource));

  signalSource.emit("SIGTERM");
  assert.deepEqual(child.killCalls, ["SIGTERM"]);

  child.emit("exit", 0, null);
  const result = await promise;
  assert.deepEqual(result, { code: 0, signal: null });
});

test("forwards SIGINT to the child", async () => {
  const child = makeFakeChild();
  const signalSource = new EventEmitter();
  const promise = runAndForwardSignals("/bin/fake", [], fakeDeps(child, signalSource));

  signalSource.emit("SIGINT");
  assert.deepEqual(child.killCalls, ["SIGINT"]);

  child.emit("exit", 0, null);
  await promise;
});

test("a second, redundant signal (e.g. terminal group-broadcast landing after an explicit forward) does not re-forward", async () => {
  const child = makeFakeChild();
  const signalSource = new EventEmitter();
  const promise = runAndForwardSignals("/bin/fake", [], fakeDeps(child, signalSource));

  signalSource.emit("SIGINT");
  signalSource.emit("SIGINT");
  signalSource.emit("SIGTERM");
  assert.deepEqual(child.killCalls, ["SIGINT"], "only the first signal is forwarded");

  child.emit("exit", 0, null);
  await promise;
});

test("resolves with the child's own exit code when it exits cleanly with no signal involved", async () => {
  const child = makeFakeChild();
  const promise = runAndForwardSignals("/bin/fake", [], fakeDeps(child, new EventEmitter()));
  child.emit("exit", 7, null);
  const result = await promise;
  assert.deepEqual(result, { code: 7, signal: null });
  assert.deepEqual(child.killCalls, [], "nothing to forward — the child exited on its own");
});

test("resolves with code:null and the signal name when the child dies by an uncaught signal", async () => {
  const child = makeFakeChild();
  const promise = runAndForwardSignals("/bin/fake", [], fakeDeps(child, new EventEmitter()));
  child.emit("exit", null, "SIGKILL");
  const result = await promise;
  assert.deepEqual(result, { code: null, signal: "SIGKILL" });
});

test("a spawn/runtime error on the child rejects the promise rather than hanging forever", async () => {
  const child = makeFakeChild();
  const promise = runAndForwardSignals("/bin/fake", [], fakeDeps(child, new EventEmitter()));
  const boom = new Error("ENOENT: no such file or directory");
  child.emit("error", boom);
  await assert.rejects(promise, /ENOENT/);
});

test("signal listeners are removed after exit — no listener leak, no re-entrancy on late signals", async () => {
  const child = makeFakeChild();
  const signalSource = new EventEmitter();
  const promise = runAndForwardSignals("/bin/fake", [], fakeDeps(child, signalSource));

  assert.equal(signalSource.listenerCount("SIGINT"), 1);
  assert.equal(signalSource.listenerCount("SIGTERM"), 1);

  child.emit("exit", 0, null);
  await promise;

  assert.equal(signalSource.listenerCount("SIGINT"), 0);
  assert.equal(signalSource.listenerCount("SIGTERM"), 0);

  // A signal arriving after the child is long gone must not throw or do
  // anything observable (there is nothing left listening).
  assert.doesNotThrow(() => signalSource.emit("SIGTERM"));
});

test("a forward landing on an already-exited child (kill() throwing ESRCH-shaped) does not reject or hang", async () => {
  const child = makeFakeChild();
  child.kill = () => {
    throw Object.assign(new Error("kill ESRCH"), { code: "ESRCH" });
  };
  const signalSource = new EventEmitter();
  const promise = runAndForwardSignals("/bin/fake", [], fakeDeps(child, signalSource));

  signalSource.emit("SIGTERM");
  child.emit("exit", 0, null);

  const result = await promise;
  assert.deepEqual(result, { code: 0, signal: null });
});

test("passes execPath/args and stdio through to spawnFn unchanged", async () => {
  const child = makeFakeChild();
  let capturedArgs = null;
  const spawnFn = (execPath, args, options) => {
    capturedArgs = { execPath, args, options };
    return child;
  };
  const promise = runAndForwardSignals("/path/to/local-rag-proxy", ["--foo", "bar"], {
    spawnFn,
    signalSource: new EventEmitter(),
  });
  child.emit("exit", 0, null);
  await promise;

  assert.equal(capturedArgs.execPath, "/path/to/local-rag-proxy");
  assert.deepEqual(capturedArgs.args, ["--foo", "bar"]);
  assert.equal(capturedArgs.options.stdio, "inherit");
});
