"use strict";

// Gates tests that shell out to the real `claude` CLI — not every
// environment running this suite has it on PATH (this project's own Rust
// CI does not). Mirrors the `LOCAL_RAG_TEST_MODEL_HOME`-style opt-in tier
// this repository already uses for other machine-dependent real-binary
// tests (see `crates/models`/`crates/generate`).

const { spawnSync } = require("node:child_process");

let cached = null;

function claudeCliAvailable() {
  if (cached === null) {
    const result = spawnSync("claude", ["--version"], { encoding: "utf8" });
    cached = result.status === 0;
  }
  return cached;
}

module.exports = { claudeCliAvailable };
