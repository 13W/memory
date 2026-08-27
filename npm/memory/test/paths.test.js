"use strict";

// The data-directory formula, which exists only to agree with another
// implementation (T22-08). Every case below is one of the four ways this can
// disagree with `crates/core/src/paths/mod.rs:180-221` while still looking
// like a reasonable data directory, so each is pinned separately rather than
// folded into one happy-path assertion.
//
// The formula was also checked against the real Rust binary at execution time
// (`local-rag init` under a temp `LOCAL_RAG_HOME`, then under `XDG_DATA_HOME`
// alone, then under neither), which is the check these tests cannot be: the
// npm suite must not require a built `target/release/local-rag`.

const { test } = require("node:test");
const assert = require("node:assert/strict");

const { dataDir, storeRoot, installDir, PathsError } = require("../src/paths.js");

const POSIX = { platform: "linux", homedir: () => "/home/u" };
const MAC = { platform: "darwin", homedir: () => "/Users/u" };
const WIN = { platform: "win32", homedir: () => "C:\\Users\\u" };

test("LOCAL_RAG_HOME is the data directory itself, not its parent", () => {
  assert.equal(dataDir({ LOCAL_RAG_HOME: "/srv/lr" }, POSIX), "/srv/lr");
  // The `local-rag` component belongs to the store root, not to data_dir —
  // `StoreLayout::resolve` adds it (`paths/mod.rs:267-270`).
  assert.equal(storeRoot({ LOCAL_RAG_HOME: "/srv/lr" }, POSIX), "/srv/lr/local-rag");
});

test("LOCAL_RAG_HOME wins over XDG_DATA_HOME and over the home fallback", () => {
  const env = { LOCAL_RAG_HOME: "/srv/lr", XDG_DATA_HOME: "/srv/xdg" };
  assert.equal(dataDir(env, POSIX), "/srv/lr");
});

test("LOCAL_RAG_HOME is taken as given even when relative — XDG_DATA_HOME is not", () => {
  // The asymmetry is deliberate on the Rust side: `LOCAL_RAG_HOME` skips the
  // absolute-path test that `XDG_DATA_HOME` must pass. Copying only the
  // symmetric-looking half would be a silent divergence.
  assert.equal(dataDir({ LOCAL_RAG_HOME: "rel/ative" }, POSIX), "rel/ative");
  assert.equal(dataDir({ XDG_DATA_HOME: "rel/ative" }, POSIX), "/home/u/.local/share");
});

test("an empty variable counts as unset, as the XDG spec requires", () => {
  // `env.X ?? fallback` would resolve to "" here and put the store at the
  // filesystem root; only `||` (or this explicit test) gets it right.
  assert.equal(dataDir({ LOCAL_RAG_HOME: "", XDG_DATA_HOME: "" }, POSIX), "/home/u/.local/share");
  const otherHome = { ...POSIX, homedir: () => "/h" };
  assert.equal(dataDir({ LOCAL_RAG_HOME: "" }, otherHome), "/h/.local/share");
});

test("an absolute XDG_DATA_HOME is honoured", () => {
  assert.equal(dataDir({ XDG_DATA_HOME: "/srv/xdg" }, POSIX), "/srv/xdg");
});

test("macOS takes the POSIX branch — never ~/Library/Application Support", () => {
  // spec 02 §2.1 says so in as many words, and the real binary was observed
  // creating `<home>/.local/share/local-rag` on this Mac.
  assert.equal(dataDir({}, MAC), "/Users/u/.local/share");
  assert.equal(storeRoot({}, MAC), "/Users/u/.local/share/local-rag");
});

test("win32 uses LOCALAPPDATA and never consults the home directory", () => {
  const env = { LOCALAPPDATA: "C:\\Users\\u\\AppData\\Local" };
  assert.equal(dataDir(env, WIN), "C:\\Users\\u\\AppData\\Local");
  assert.equal(storeRoot(env, WIN), "C:\\Users\\u\\AppData\\Local\\local-rag");
  // A home directory that would resolve on POSIX must not rescue win32.
  assert.throws(() => dataDir({}, WIN), (err) => err instanceof PathsError);
});

test("paths are joined in the target platform's flavour, not the host's", () => {
  // D-055's lesson: `npmGlobalNodeModules` had to name `path.win32`/`path.posix`
  // explicitly, because the ambient `path` module joins with the host's
  // separator and a cross-platform formula test then passes for the wrong
  // reason. This assertion is the one that fails without it.
  assert.equal(
    installDir("win32-x64", { LOCAL_RAG_HOME: "C:\\lr" }, WIN),
    "C:\\lr\\local-rag\\bin\\x86_64-pc-windows-msvc",
  );
  assert.equal(
    installDir("linux-x64", { LOCAL_RAG_HOME: "/lr" }, POSIX),
    "/lr/local-rag/bin/x86_64-unknown-linux-gnu",
  );
});

test("the install directory is keyed by target triple, not by platform key", () => {
  // Two architectures under one home — a Rosetta shell and a native one on the
  // same Mac — must not overwrite each other's binaries.
  const env = { LOCAL_RAG_HOME: "/lr" };
  assert.notEqual(installDir("darwin-arm64", env, MAC), installDir("darwin-x64", env, MAC));
  assert.match(installDir("darwin-arm64", env, MAC), /aarch64-apple-darwin$/);
});

test("a platform with no release target is a typed error, not a bad path", () => {
  assert.throws(
    () => installDir("win32-arm64", { LOCAL_RAG_HOME: "/lr" }, WIN),
    (err) => err instanceof PathsError && err.kind === "unsupported-platform",
  );
  assert.throws(
    () => installDir("freebsd-x64", { LOCAL_RAG_HOME: "/lr" }, POSIX),
    (err) => err instanceof PathsError && err.kind === "unsupported-platform",
  );
});

test("no base directory at all is a typed error naming the way out", () => {
  assert.throws(
    () => dataDir({}, { platform: "linux", homedir: () => "" }),
    (err) =>
      err instanceof PathsError &&
      err.kind === "no-base-dir" &&
      /LOCAL_RAG_HOME/.test(err.message),
  );
});
