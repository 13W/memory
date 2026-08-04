"use strict";

// Card requirement: "package contents exclude weights and unrelated files"
// (ADR-0004/0005 `[FIXED policy]`: weights are never shipped in npm
// packages). Verified hermetically via `npm pack --dry-run --json` — local
// only, no registry contact, no publish — over a *copy* of each real
// package directory with synthetic decoy files injected, so the test
// proves both that real decoys would be excluded AND that the `files`
// allowlist is not so narrow it would also drop legitimate files.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const REPO_NPM_DIR = path.resolve(__dirname, "..", "..");

const PACKAGES = [
  { dir: "memory", expectedFiles: ["package.json", "bin/local-rag-mcp.js", "src/resolve.js"] },
  { dir: "memory-darwin-arm64", expectedFiles: ["package.json"] },
  { dir: "memory-darwin-x64", expectedFiles: ["package.json"] },
  { dir: "memory-linux-x64", expectedFiles: ["package.json"] },
  { dir: "memory-linux-arm64", expectedFiles: ["package.json"] },
  { dir: "memory-win32-x64", expectedFiles: ["package.json"] },
];

const DECOY_FILES = [
  "models/embeddinggemma-300m/model.onnx",
  "models/fake-weights.bin",
  "node_modules/leftover-dep/index.js",
  ".git/HEAD",
  "npm-debug.log",
  ".DS_Store",
];

/** Copies a real package dir into a fresh temp dir and injects decoy files. */
function preparePackageCopy(realPackageDir) {
  const dest = fs.mkdtempSync(path.join(os.tmpdir(), "lr-pack-"));
  fs.cpSync(realPackageDir, dest, { recursive: true });
  for (const decoy of DECOY_FILES) {
    const decoyPath = path.join(dest, decoy);
    fs.mkdirSync(path.dirname(decoyPath), { recursive: true });
    fs.writeFileSync(decoyPath, "decoy content — must never ship");
  }
  return dest;
}

/** @returns {string[]} paths (relative to the package root) `npm pack` would ship */
function packedFileList(packageDir) {
  const out = execFileSync("npm", ["pack", "--dry-run", "--json"], {
    cwd: packageDir,
    encoding: "utf8",
  });
  const parsed = JSON.parse(out);
  // npm's own `pack --json` shape has varied across versions: some print a
  // top-level array (`[{...}]`), this one (npm 12.x) prints an object
  // keyed by package name (`{"@13w/...": {...}}`) — accept either so this
  // test does not become a version-pinned artifact of the CI/dev npm.
  const report = Array.isArray(parsed) ? parsed[0] : Object.values(parsed)[0];
  return report.files.map((f) => f.path);
}

for (const { dir, expectedFiles } of PACKAGES) {
  test(`${dir}: npm pack excludes injected weights/unrelated-file decoys`, () => {
    const realDir = path.join(REPO_NPM_DIR, dir);
    const copy = preparePackageCopy(realDir);

    const files = packedFileList(copy);

    for (const decoy of DECOY_FILES) {
      assert.ok(!files.includes(decoy), `${dir}: decoy "${decoy}" must not be packed, got: ${files.join(", ")}`);
    }
    // Nothing under the decoy top-level directories should sneak in either
    // (npm sometimes reports directory-relative paths differently).
    assert.ok(
      !files.some((f) => f.startsWith("models/")),
      `${dir}: no file under models/ (weights) may ever be packed`,
    );
    assert.ok(
      !files.some((f) => f.startsWith("node_modules/")),
      `${dir}: no vendored node_modules/ content may be packed`,
    );

    fs.rmSync(copy, { recursive: true, force: true });
  });

  test(`${dir}: npm pack still includes every legitimate file (the allowlist is not over-restrictive)`, () => {
    const realDir = path.join(REPO_NPM_DIR, dir);
    const copy = preparePackageCopy(realDir);

    const files = packedFileList(copy);
    for (const expected of expectedFiles) {
      assert.ok(files.includes(expected), `${dir}: expected "${expected}" to be packed, got: ${files.join(", ")}`);
    }

    fs.rmSync(copy, { recursive: true, force: true });
  });
}

test("the launcher's own README.md is not required for the launcher to run, but is still packed if present (informational, not a decoy)", () => {
  const realDir = path.join(REPO_NPM_DIR, "memory");
  const copy = preparePackageCopy(realDir);
  const files = packedFileList(copy);
  // README.md is npm's own always-included file regardless of `files` —
  // documenting the existing behavior rather than asserting a new rule.
  assert.ok(files.includes("README.md"));
  fs.rmSync(copy, { recursive: true, force: true });
});
