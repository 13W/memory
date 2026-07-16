# parser fixtures — gap-only

v1 has **no golden tree-sitter chunking fixtures** (`source file → expected units {kind, span,
locator}`). The v1 parser (`src/indexer/parser.ts`) exists and is exercised only indirectly
through the 49-query search benchmark; there are no per-language unit goldens to import.

Adjacent v1 material was imported into other families rather than here:

- import resolution (`src/indexer/resolver.test.ts`) feeds the dependency graph, which is
  **deferred** in v0 (`find_usages`/`get_dependencies` parity — 15 §3). Recorded in
  `../manifest.json` under `deferred`, not as a v0 fixture.
- `.gitignore`/`.ignore` skip semantics → `reconcile` family (skip policy).
- malformed LLM-output parsing (`scripts/test-parser-fix.ts`) → `adversarial` family.

The blocking gap is registered as **GAP-01** in `../manifest.json`. Real parser unit goldens
are authored once the v0 language set is fixed (O4 / T04-01) in tasks T04-03…T04-06.
No fixtures are invented here.
