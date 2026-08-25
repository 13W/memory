# ADR-0001: First-release language set (v0)

## Status

Accepted — 2026-07-20.

Closes open question **O4 "First-release language set"**
([spec 15 §4](../specification/15-roadmap.md)). Delivered by task **T04-01**
([group 04](../implementation-plan/groups/04-parsing.md)). This is the first ADR
in the repository; it establishes the convention (`docs/adr/NNNN-title.md`,
Nygard-style: Status / Context / Decision / Consequences, English to match the
`docs/specification/*` files it amends) reused by later decision records
(T10-05 dense backend, T11/T14-07 embeddings/memory).

## Context

The MVP scope fixes the *number* of parsed languages but not the *set*:
[spec 15 §2](../specification/15-roadmap.md) says "tree-sitter for 2–3 languages
`[OPEN which]`", and [idea.md §16](../idea.md) mirrors it ("tree-sitter 2–3
языка"). O4 is resolved "by benchmark corpus needs". The T04-01 card asks the ADR
to choose 2–3 languages weighing the available 49-query corpus, user value, and
parser maturity / platform support.

What the data actually says:

- The imported behavioral corpus is **single-language**. All 49 queries in
  [`fixtures/search/corpus.json`](../../fixtures/search/corpus.json) target the v1
  code base, which is 100% TypeScript; the v1 provenance in
  [`fixtures/manifest.json`](../../fixtures/manifest.json) records
  `source.language = "TypeScript"`. The corpus has no `language` field and there
  is no multi-language distribution to mine.
- The captured baseline is likewise TypeScript-only: the code-only
  `embeddinggemma:300m` run indexed 544 chunks over the v1 TypeScript tree
  (Hit@1 0.59 / Hit@3 0.80 / Hit@5 0.84 / MRR 0.70). It is the *only* measurable
  input to the search gates (T12-05).
- `GAP-01` in the manifest explicitly defers per-language golden parser fixtures
  to this decision: "The v0 language set is `[OPEN]` (O4). Parser unit goldens are
  authored once the language set is fixed" (`resolves_in: T04-01 (O4/ADR),
  T04-03..T04-06`).
- The provisional config default was `["typescript", "javascript"]`
  ([spec 02 §3.1](../specification/02-architecture.md)), carried as an `[OPEN]`
  placeholder, not a closed answer.

## Decision

The first-release (v0) language set is **TypeScript, JavaScript, Rust** — three
languages, within the `[FIXED]` "2–3" bound.

Rationale per language:

- **TypeScript** — mandatory. It is the only language with a measurable benchmark
  and captured baseline; excluding it would make the search gates (T12-05)
  unmeasurable. This is the direct reading of O4's "resolved by benchmark corpus
  needs".
- **JavaScript** — shares a tree-sitter grammar family with TypeScript
  (`tree-sitter-typescript` is built on `tree-sitter-javascript`): minimal
  additional parser risk, maximal reuse, high user value, and the broadest
  platform support.
- **Rust** — dogfooding. It lets local-rag index its own code base, and the
  tree-sitter Rust grammar is mature and cross-platform.

Extension mapping (the precise language-by-path selector is T04-02; this ADR only
fixes the set):

| Language   | Extensions                |
| ---------- | ------------------------- |
| typescript | `.ts` `.tsx` `.mts` `.cts` |
| javascript | `.js` `.jsx` `.mjs` `.cjs` |
| rust       | `.rs`                     |

Scope boundary. This set names the languages that get **tree-sitter symbol
parsers**. Structured non-code text (YAML, JSON) and plain text are handled by the
language-agnostic unit kinds `config_section | text_section | fallback_chunk`
([spec 06 §2.1](../specification/06-reconcile-and-fts.md)); they are **not** part
of the O4 language set. The v1 parser's YAML/JSON handling (referenced in corpus
queries `sc-22`/`sc-23`) belongs to that universal path — not to this decision.

> **Amended 2026-08-25 by [ADR-0012](0012-universal-file-indexing-path.md)
> (D-098).** The sentence above originally said that path was "specified later
> (T04-06 / groups 05 and 08)". Every one of those addresses was wrong: T04-06 is
> deterministic parsed-unit persistence, and groups 05 and 08 both closed `PASS`
> having deferred the path by design. The pointer dangled, so a `[FIXED]`
> requirement of spec 06 §2.1 had no owner in any card until D-098 — and while it
> had none, a file with no v0 language was written **nowhere**, which cost 3 455
> of one real repository's files and all 119 `.md` files of this one. ADR-0012
> owns the universal path now. This decision's language *set* is unchanged by it:
> `UniversalKind` is a sibling of `LanguageId`, deliberately not three more
> variants of it, precisely so the set fixed here stays closed.

Known, accepted limitation. There is **no benchmark corpus for JavaScript or
Rust** in v0, so retrieval quality on those languages is not measured; the
TypeScript gate remains the primary quality signal. Their parser adapters are
still held to the determinism and byte-span goldens of
[spec 14 §5](../specification/14-acceptance-and-testing.md) in T04-04/T04-05. This
limitation is recorded here deliberately rather than left implicit.

## Consequences

- The downstream language tasks are unblocked with concrete targets: **T04-03**
  TypeScript adapter + goldens, **T04-04** JavaScript adapter + goldens,
  **T04-05** Rust adapter + goldens. T04-05 is no longer "N/A by ADR".
- The parser core stays language-agnostic: the choice lives in data/config, not in
  the parser abstraction (T04-02), honoring the O4 rule "parser core не привязывать
  к конкретному набору" from the traceability matrix.
- The provisional config default becomes final: `index.languages =
  ["typescript", "javascript", "rust"]`
  ([spec 02 §3.1](../specification/02-architecture.md) and
  `crates/core/src/config`), and the `[OPEN]` markers for the language set are
  removed across the specification.
- Adding languages after v0 is additive: a new language is a new adapter + query
  set + goldens, with no schema or identity change (`parser_fingerprint` already
  keys on `lang`/`grammar` version — [spec 03 §2.3.1](../specification/03-data-model.md)).
- Two guards enforce this decision in CI: an ADR link-check
  (`crates/xtask/tests/adr_links.rs`) and a corpus/manifest language-coverage
  invariant (`crates/index/tests/language_coverage.rs`) asserting the benchmark
  language is in the set and the non-benchmark languages are acknowledged here.
