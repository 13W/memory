# parser fixtures

Implementation-neutral golden parser fixtures (spec 14 §1.1): a source file maps
to its expected units (`source file → expected {unit_kind, byte span, local_name,
kind, anchor, parent}`) and its classified unresolved references. The corpus is
multi-language — each case declares its `language`, and the integration test routes
it to the matching adapter. Byte spans are offsets into the exact source bytes; the
`anchor` is the readable `p:<syntax_path>` / `o:<ordinal>` form (ADR-0002).
Implementation-specific values — the `sig` hash and the full serialized
`syntax_locator` — are deliberately **not** stored here (they would defeat
neutrality and churn per language); they are pinned in crate-local goldens in
`crates/index` instead.

Artifact: [`index.json`](index.json), validated by
[`../schema/parser.schema.json`](../schema/parser.schema.json) and consumed by the
Rust integration test `crates/index/tests/parse_fixtures.rs`.

## Status

- **TypeScript** — authored in **T04-03** (`tree-sitter` `tsx` adapter, ADR-0002),
  covering the `syntax`, `error`, `empty`, and `unicode` categories.
- **JavaScript** — authored in **T04-04** (`tree-sitter-javascript` adapter,
  ADR-0002), covering the same categories.
- **Rust** — authored in **T04-05** (`tree-sitter-rust` adapter, ADR-0002),
  covering the same categories. This completes the v0 language set and closes
  **GAP-01** (`../manifest.json`).

v1 had no golden tree-sitter chunking fixtures (the v1 parser `src/indexer/parser.ts`
was exercised only through the 49-query benchmark), so these are authored, not
imported. Adjacent v1 material lives in other families: import resolution feeds the
post-v0 dependency graph (recorded under `deferred` in `../manifest.json`);
`.gitignore` skip semantics went to the `reconcile` family; malformed-output
parsing went to `adversarial`.
