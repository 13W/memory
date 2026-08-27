"use strict";

// Card requirement: "package contents exclude weights and unrelated files"
// (ADR-0004/0005 `[FIXED policy]`: weights are never shipped in npm
// packages). Verified hermetically via `npm pack --dry-run --json` — local
// only, no registry contact, no publish — over a *copy* of the real package
// directory with synthetic decoy files injected, so the test proves both that
// real decoys would be excluded AND that the `files` allowlist is not so
// narrow it would also drop legitimate files.
//
// ONE PACKAGE, not six (T22-11). This file used to walk `memory` plus the five
// `@13w/memory-<platform>` packages; ADR-0013 replaced that whole channel with
// GitHub release assets, and this test is also the card's acceptance —
// "`npm pack --dry-run` gives one package of the expected shape". Its five
// platform entries went with the directories they described.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const REPO_NPM_DIR = path.resolve(__dirname, "..", "..");

const PACKAGES = [
  {
    dir: "memory",
    // `scripts/install.js` is listed because a file that ships only in the
    // checkout is worse than no file: the package would look complete and heal
    // nothing. The npm `scripts` *field* — the lifecycle hooks — is a separate
    // thing and belongs to T22-10.
    //
    // Named individually because each one is load-bearing for a different
    // reason: all four `bin/` stubs are what `postinstall` replaces with native
    // binaries, and `src/locate.js` is the resolver every command goes through.
    // The exhaustive check — that no module can be added to the checkout and
    // silently not ship — is derived from the directory below rather than
    // maintained here by hand.
    expectedFiles: [
      "package.json",
      "bin/local-rag",
      "bin/local-rag-proxy",
      "bin/local-rag-hook",
      "bin/local-rag-tui",
      "src/locate.js",
      "scripts/install.js",
      "scripts/postinstall.js",
      "scripts/prepack.js",
    ],
  },
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

test("every file the checkout actually has under bin/, src/ and scripts/ is packed", () => {
  // The list above names what must ship; this names what exists. A new module
  // added to `src/` and forgotten in the `files` allowlist would leave a
  // package that looks complete and throws `MODULE_NOT_FOUND` on the user's
  // first command — the exact failure `expectedFiles` cannot catch, because a
  // hand-maintained list does not grow by itself.
  const realDir = path.join(REPO_NPM_DIR, "memory");
  const packed = new Set(packedFileList(preparePackageCopy(realDir)));
  const onDisk = [];
  for (const sub of ["bin", "src", "scripts"]) {
    for (const name of fs.readdirSync(path.join(realDir, sub))) {
      // Installer artefacts are not source; `prepack` refuses to pack a bin/
      // that has been installed into, and that is its own test below.
      if (name.startsWith(".")) continue;
      onDisk.push(`${sub}/${name}`);
    }
  }
  assert.ok(onDisk.length > 10, "a near-empty checkout would satisfy this trivially");
  for (const file of onDisk) {
    assert.ok(packed.has(file), `${file} exists in the checkout but npm pack would not ship it`);
  }
});

test("a bin/ that has been installed into cannot be packed at all", () => {
  // The design has `postinstall` replace the stubs in `bin/` with native
  // binaries — in a checkout, that is the very directory `npm pack` reads. So
  // publishing from a machine that has ever installed the package would put one
  // platform's binaries into a tarball every platform downloads. `prepack` is
  // the only thing standing between those two facts.
  //
  // `prepack` runs for `npm pack --dry-run` too, and a non-zero exit aborts the
  // pack before a tarball exists. Measured, not assumed: npm's own
  // `libnpmpack` awaits the script before `pacote.tarball`, and `dryRun` gates
  // only the final write.
  const copy = preparePackageCopy(path.join(REPO_NPM_DIR, "memory"));

  // Not "#!" — which is the whole test. The rule is a whitelist, so a Mach-O,
  // an ELF and a PE all fail it without anyone enumerating magic numbers.
  fs.writeFileSync(path.join(copy, "bin", "local-rag-proxy"), Buffer.from([0xcf, 0xfa, 0xed, 0xfe, 0x0c]));
  fs.writeFileSync(path.join(copy, "bin", ".local-rag-install.json"), "{}");

  assert.throws(
    () => packedFileList(copy),
    (err) => {
      const output = `${err.stdout ?? ""}${err.stderr ?? ""}${err.message}`;
      assert.match(output, /refusing to pack/);
      assert.match(output, /local-rag-proxy/);
      assert.match(output, /installer artefact/);
      return true;
    },
  );

  fs.rmSync(copy, { recursive: true, force: true });
});

test("the launcher's own README.md is not required for the launcher to run, but is still packed if present (informational, not a decoy)", () => {
  const realDir = path.join(REPO_NPM_DIR, "memory");
  const copy = preparePackageCopy(realDir);
  const files = packedFileList(copy);
  // README.md is npm's own always-included file regardless of `files` —
  // documenting the existing behavior rather than asserting a new rule.
  assert.ok(files.includes("README.md"));
  fs.rmSync(copy, { recursive: true, force: true });
});
