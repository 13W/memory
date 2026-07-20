# ADR-0002: SyntaxLocator derivation semantics (v0)

## Status

Accepted — 2026-07-20.

Resolves the **`SyntaxLocator` derivation** half of open question **O7 "Final
`SyntaxLocator` / graph semantics"**
([spec 15 §4](../specification/15-roadmap.md)). Delivered by task **T04-03**
([group 04](../implementation-plan/groups/04-parsing.md)), the first tree-sitter
parser adapter (TypeScript, per [ADR-0001](0001-first-release-language-set.md)).
The **graph** half of O7 (`resolved_graph_edge.edge_kind`, `find_usages` /
`get_dependencies`) is post-v0 and stays open. Follows the ADR convention
established by ADR-0001.

## Context

[spec 03 §2.4](../specification/03-data-model.md) fixes the `SyntaxLocator`
**shape** and **serialization** (`{language, anchor, signature_fingerprint,
blob_id}`; a canonical, path-free `key=value` string over `{anchor, blob, lang,
sig}` where `anchor` is `p:<syntax_path>` or `o:<local_ordinal>`), realized by
T04-02 in `crates/index/src/parse/locator.rs`. But it leaves the finer
**derivation** of `syntax_path` and `signature_fingerprint` from a real parse
tree `[OPEN]` (O7), explicitly deferred to the parser adapters (T04-03+).

The card requires the adapter to produce `symbol/file/fallback` units with byte
spans, parents, and unresolved references, and requires determinism: same
`(content, parser_fingerprint)` ⇒ byte-identical unit sets
([spec 14 §5](../specification/14-acceptance-and-testing.md);
[spec 06 §2.1](../specification/06-reconcile-and-fts.md)). The
`parser_fingerprint` also has a first-class `queries=<query_version>` dimension,
so extraction is a **versioned** artifact.

This ADR fixes the v0 derivation. It is scoped to what makes locators
deterministic and reasonably stable; it does not attempt final graph semantics.

## Decision

### 1. Parse output is a pure function

`LanguageParser::parse(&[u8]) -> ParseOutput` is a pure, deterministic function of
the source bytes and the compiled grammar/query. It mints no ids, reads no clock,
and touches no database. Persistence — minting `unit_id`, deriving each unit's
`blob_id` from the normalized text of `source_blob[span]`, atomic create/reuse,
dedup — is T04-06. A unit's `parent` is carried as an index into `ParseOutput.units`
(the parser has no ids); T04-06 maps indices to minted ids.

Byte spans are `start_byte`/`end_byte` into the **exact source bytes** (the
`source_blob`), never normalized text. A unit's later `blob_id` hashes the
**normalized** text of the same slice — a deliberately different byte world.

### 2. `unit_kind` mapping (TypeScript, v0)

`symbol` for every extracted declaration; `file` for the single whole-file unit;
`fallback_chunk` for each outermost ERROR/MISSING region. `config_section` /
`text_section` are **not applicable** to a code language — they belong to the
later universal / non-code path (ADR-0001 scope boundary,
[spec 06 §2.1](../specification/06-reconcile-and-fts.md)). This discharges the
card's "as applicable".

Extracted symbol constructs (`query_version = 1`): function / generator /
overload-signature declarations; class / abstract class; interface; enum; type
alias; namespace / module; method / method-signature / abstract-method-signature;
and **module-scope** function/class-valued `const`/`let`/`var` bindings (the
`export const f = () => {}` idiom). The capture set is the versioned query
contract; broadening it later is a `queries=` rebuild event.

### 3. `syntax_path` (the `p:` anchor)

A unit uses a **named route** iff the unit and every enclosing declaration
ancestor have a safe name. The route is `<lang_kind>:<name>` per link, outermost
first, joined with `/` (e.g. `class:Foo/method:bar`, matching the spec example).

A **safe name** is the text of an identifier-family name node
(`identifier`/`type_identifier`/`property_identifier`/…) that contains none of the
reserved bytes `;`, `=`, `/`, `:` and no whitespace/control — which every
ECMAScript identifier satisfies, so the `SyntaxLocator::serialize` delimiter
invariant holds by construction (a defensive character check backs it).

The whole-file unit uses the fixed safe anchor `p:file`.

### 4. Ordinal fallback (the `o:` anchor)

When a unit or any ancestor is anonymous or has a non-safe name (anonymous
`export default function`, an arrow assigned to a non-identifier target, a class
expression, a computed or string method name), the unit uses
`o:<local_ordinal>`, where the ordinal is its 0-based position among its parent's
direct child units in canonical order (top-level units are ordered among
themselves). Fallback (error) chunks always use an ordinal.

The ordinal need not be globally unique — the stored row is made distinct by
`span_start`/`span_end` in the `parsed_unit` UNIQUE key and by `sig`/`blob` in the
serialized locator; the ordinal is only a deterministic positional handle.

### 5. `signature_fingerprint` (the `sig` field)

`sig` is a domain-separated BLAKE3 hash
(`local_rag_core::identity::domain::signature_fingerprint`, new `Domain`
variant, [spec 03 §1.2](../specification/03-data-model.md)) of a **canonical
descriptor** assembled from the parse subtree only: language, unit kind,
language kind, local name, sorted modifiers, and per-kind structural fields
(type-parameter / parameter / return-type text; class/interface heritage; body
member count; a type alias's aliased type text; a `const` value's node kind).
Fields are joined by the ASCII Unit Separator and hashed as **one** domain field,
so the descriptor's internal shape may evolve within a `queries=`/`grammar=`
rebuild event without a `HASH_SCHEMA_VERSION` bump.

Hashing (not a readable descriptor) was chosen so `sig` is delimiter-safe by
construction (64 lowercase hex, no `;`/`=`), centralizes delimiter-safety for
future languages, and keeps the locator short; `sig` is "carried opaquely in v0"
per spec, and the neutral parser fixtures pin the readable facts (kind, span,
route) rather than the hash. **Considered and rejected:** a readable canonical
descriptor as the `sig` value — it pushes per-language escaping logic into every
adapter and lengthens the locator for no v0 benefit.

Overload signatures with different parameters therefore get distinct `sig`s (so
distinct locators under a shared route); a group that would collide on *both*
route and `sig` is demoted wholesale to ordinal anchors.

### 6. Canonical order

`ParseOutput.units` is totally ordered by ascending `span.start`, then descending
`span.end` (an enclosing unit before the units it contains), then `unit_kind`,
`lang_kind`, `local_name`, `sig`. This is insertion-order-independent, guarantees
`parent < child` by index, and gives T04-06 its canonical ordering for free.

### 7. Grammar variant and version binding

The **`tsx`** grammar is used for every TypeScript extension. `parse` never sees
the path and the `parser_fingerprint` is identical across `.ts`/`.tsx`/`.mts`/
`.cts`, so a single grammar is mandatory for determinism; `tsx` is the practical
superset (parses JSX; the only regression is the rare `<T>expr` cast, for which
`expr as T` exists). Splitting grammars per extension would need a new fingerprint
dimension — a rebuild event — and is deferred.

`grammar_version` and `query_version` stay at **1**: they are our
boundary-version counters, not the upstream crate semver. T04-03 links the real
grammar and **reconciles `@1` to the pinned crates `tree-sitter 0.24` /
`tree-sitter-typescript 0.23`** (recorded in `parse::fingerprint::descriptor`).
No data is persisted yet (T04-06), so the existing `fingerprint.rs` goldens stay
green; this is the deliberate, documented reconciliation the spec anticipates, not
a silent bump. `BOUNDARY_NORM_VERSION = 1` means "no boundary-shifting
normalization — the grammar parses raw bytes"; `CHUNK_POLICY_VERSION = 1` means
"fallback chunks only for outermost ERROR/MISSING spans, no size-based splitting".
Any future change to these behaviors is a deliberate version bump (a rebuild
event), guarded by the `version_constants_and_descriptors_are_pinned` tripwire.

### 8. Unresolved references

v0 extracts module specifiers only (import resolution and the dependency graph are
post-v0). `import … from "X"` → `reference_kind = import`; `import type … from
"X"` → `type_import`; `export … from "X"` / `export * from "X"` → `reexport`. The
`source_unit` is the file unit; references are ordered by source position.

## Consequences

- The `SyntaxLocator` derivation half of O7 is resolved; the graph half remains
  open. [spec 15 §4](../specification/15-roadmap.md) and
  [spec 03 §2.4](../specification/03-data-model.md) are amended from `[OPEN]` to
  as-built `[SPEC]` for the derivation, citing this ADR.
- A new hash domain `signature_fingerprint` is added (additive; `HASH_SCHEMA_VERSION`
  stays 1), recorded in [spec 03 §1.2](../specification/03-data-model.md).
- The parser core stays language-agnostic: a `LanguageSpec` seam
  (grammar/query/capture-map/name/signature/reference hooks) lets T04-04
  (JavaScript) and T04-05 (Rust) reuse the shared engine — realizing ADR-0001's
  "the choice lives in data/config".
- Stability: the named route is independent of byte offsets and of unrelated
  edits, so an unchanged symbol keeps its locator across edits (the structural
  sharing gate, [spec 06 §2.1](../specification/06-reconcile-and-fts.md));
  ordinals are the documented weaker fallback for anonymous nodes.
- Known v0 limitations, recorded deliberately: symbols recovered inside an ERROR
  region may overlap a `fallback_chunk`; a class expression's heritage is not
  folded into a `const` binding's `sig`; inline `import { type A }` specifiers are
  treated as value imports. None affect determinism.
