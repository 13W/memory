# ADR-0015: Post-v0 language expansion — Python, Go, Bash, YAML, TOML

## Status

Accepted — 2026-09-04.

Opens new scope by the owner's explicit product decision, and **amends
[ADR-0001](0001-first-release-language-set.md)'s language set** rather than superseding it.
Realized by group 24 with gate `G24`
([`groups/24-language-expansion.md`](../implementation-plan/groups/24-language-expansion.md)).
No gate `G00`–`G23` is reopened.

This ADR is the one [ADR-0012](0012-universal-file-indexing-path.md) named in advance: "A real
tree-sitter Python adapter is **a new ADR amending ADR-0001's set**, not part of this one."

## Context

### What the universal path leaves on the table

[ADR-0012](0012-universal-file-indexing-path.md) (D-098) made every scanned file land in exactly
one of `generation_file` or `skipped_file`, which closed a real hole. It did not claim to give
those files *good* units — it gave them units. Two years of that trade are now visible:

- `.py`, `.go`, `.sh` are `fallback_chunk`: line-aligned windows under `MAX_SECTION_BYTES`, with
  **no name at all**. Every unit is an ordinal anchor.
- `.yaml`, `.yml`, `.toml` are `config_section`, named by the line-scanning heuristic in
  `parse::universal::config_key`, which recognizes exactly `key:` and `key =` lines.

That heuristic was re-measured against real samples while this decision was being written, and
it misnames two of the three config formats it is supposed to serve:

```
TOML       sections: name, version, serde, name
           [package] / [dependencies] / [[bin]] are never section starts
k8s YAML   sections: apiVersion, kind, metadata, spec, apiVersion, kind
           documents separated by --- are merged into one flat key list
```

The TOML case is the sharper one: a table header is exactly the unit a reader searches for, and
it is precisely what the rule drops. The HCL case is worse still — because a block header
(`resource "aws_s3_bucket" "logs" {`) is not a `key =` line, the file's *minimum key indentation*
lands **inside** the blocks, and sections get named after generic attributes (`bucket`, `acl`)
that repeat across the whole repository.

None of this makes content unsearchable: BM25 and the dense leg run over section text, not
section names. It degrades **unit boundaries and unit names**, which is what recall ranks and
what a reader sees in a result.

### What actually loads, measured rather than assumed

A throwaway crate was built and run against this project's exact pinned core `tree-sitter
0.24.7` (`LANGUAGE_VERSION = 14`, `MIN_COMPATIBLE = 13`). The versions below are what Cargo
resolved, and each grammar parsed a real sample without error:

| Crate | Version | Grammar ABI | License | Adds transitive crates |
| --- | --- | --- | --- | --- |
| `tree-sitter-python` | 0.23.6 | 14 | MIT | none |
| `tree-sitter-go` | 0.23.4 | 14 | MIT | none |
| `tree-sitter-bash` | 0.23.3 | 14 | MIT | none |
| `tree-sitter-yaml` | 0.7.2 | 14 | MIT | none |
| `tree-sitter-toml-ng` | 0.7.0 | 14 | MIT | none |

`tree-sitter-language` is already in the tree (shared with `tree-sitter`) and `cc` is already a
build dependency of rusqlite/zstd, so all five together add **zero new packages** to the
lockfile — only dependency edges. Note that `tree-sitter-yaml` 0.7.2 is the *latest* release and
is still ABI 14; there is no reason to pin an older one.

### The `[FIXED]` bound, and why it is not violated

[idea.md §16](../idea.md) says "tree-sitter 2–3 языка" and it is `[FIXED]` at precedence #1.
ADR-0001 deliberately placed the v0 set at the top of that bound.

That sentence describes the **v0 MVP**, and this decision does not touch v0. ADR-0001 already
settled the forward case in as many words: "Adding languages after v0 is additive: a new
language is a new adapter + query set + goldens, with no schema or identity change". So
`idea.md` is **not rewritten** and no design revision is issued — the bound stays true of the
release it describes, and this is post-v0 additive scope on top of it.

This paragraph exists because the ambiguity is genuine. Leaving the reasoning implicit would
invite the argument to be reopened by the next reader; recording it closes it.

## Decision

### 1. Five languages join the set, post-v0

`LanguageId` gains `Python`, `Go`, `Bash`, `Yaml`, `Toml`. The v0 set fixed by ADR-0001 is
unchanged in meaning — it remains the set that shipped v0 — and this decision extends it for the
releases after.

| Language | Extensions |
| ---------- | ------------------------- |
| python | `.py` `.pyi` |
| go | `.go` |
| bash | `.sh` `.bash` |
| yaml | `.yaml` `.yml` |
| toml | `.toml` |

### 2. An extension belongs to exactly one selector

`select_dialect` consults `select_language` first and only then `universal_kind`, so the moment
`yaml`/`yml`/`toml` join the extension table, their entries in `CONFIG_EXTENSIONS` become
unreachable. Dead entries are two sources of truth for one question, which is how the next
maintainer gets a wrong answer cheaply.

**Each language card removes its extensions from `CONFIG_EXTENSIONS` in the same commit that
adds them to `select_language`,** and group 24 adds a test asserting that no extension appears
in both tables. `CONFIG_EXTENSIONS` keeps `json`, `jsonc`, `json5`, `ini`, `cfg`, `conf`,
`properties`, `env`, `tf`, `tfvars`, `hcl`, `tfstate`.

### 3. YAML and TOML keep `config_section`; they do not become `symbol`

This is the decision that keeps the unit kinds meaningful, and it is available because the
engine takes `unit_kind` straight from the adapter: `CaptureRole::Decl { unit_kind, lang_kind }`
is passed through by `parse_with` (`crates/index/src/parse/adapter/mod.rs:118-131`) rather than
being fixed to `Symbol`. The three v0 adapters all happen to return `Symbol`; nothing requires it.

So the YAML and TOML adapters emit `UnitKind::ConfigSection` for their sections — a **grammar**
deciding the boundaries and names of a config section, in place of a line scanner. A YAML key is
not a symbol and calling it one to fit the language path would be a lie told for implementation
convenience.

Spec 06 §2.1's `[FIXED]` kind set is untouched: it fixes *which kinds exist and that all are
indexed*, never which file produces which kind. `parsed_unit.unit_kind`'s `CHECK` over five
kinds (`crates/store/src/code/mod.rs`) is likewise untouched — **no language may invent a sixth
kind.**

### 4. Grammars are pinned at ABI 14, and the pin is load-bearing

Every grammar is pinned to its `0.23.x`/ABI-14 line, for the reason the three existing pins
already record: `tree-sitter 0.24` supports language ABI 14 at most, and an ABI-15 grammar is
not rejected loudly — `parse_with` degrades to a file-only parse. Each adapter's
`grammar_loads_and_extracts_symbols` test is the tripwire that turns that silence into a failure.

### 5. What this decision does *not* open

- **HCL / Terraform / Terragrunt.** Measured: `tree-sitter-hcl` 1.1.0 is ABI 15 and the core
  refuses it — `Incompatible language version 15. Expected minimum 13, maximum 14`. No ABI-14
  release exists, so the only route is lifting the core, which is a separate decision. Recorded
  here so the next reader does not re-derive it: upstream `tree-sitter` is at 0.27.0, and its
  `MIN_COMPATIBLE_LANGUAGE_VERSION` is 13, so such a lift would be backward compatible with the
  grammars pinned by this ADR.
- **Kubernetes / Kustomize.** Not a language. A manifest is YAML; what makes it Kubernetes is a
  schema (`apiVersion`/`kind`/`metadata.name`), not a syntax, and any YAML grammar returns the
  same key names. Schema-aware naming belongs to whoever owns the YAML adapter's naming rule, as
  a later, separate change.
- **A generic `tags.scm`-driven adapter.** The tree-sitter ecosystem has a symbol-extraction
  query convention (`queries/tags.scm`, the `tree-sitter-tags` crate behind GitHub's code
  navigation) that upstream ships for python, go, java, ruby, c, cpp, c-sharp, php and scala. It
  would make a new language a query file rather than 400 lines of Rust. It is deliberately **not**
  adopted here: its capture vocabulary is poorer than the existing hand-written queries (no
  `impl`, `mod`, `union`, `type_alias`, `macro_definition` for Rust), and switching the existing
  languages to it would be a regression plus a `queries=` rebuild event for every language. It
  remains a live option for a future decision, and upstream `tags.scm` files are used as a
  *reference* for capture design by group 24's cards.

### 6. One card per language, config formats last

Group 24 spends one card per language, because each is an independent result with its own
grammar, query, goldens and fixtures — the split `TASK-TEMPLATE.md` requires. Ordering:
Python, Bash, Go, then TOML and YAML.

The config formats come last on purpose. They are the only two that **replace** working behavior
rather than adding missing behavior, and they carry most of the re-index cost; sequencing them
behind the others means an unfavourable measurement can stop the group with everything before it
already shipped and green.

## Consequences

- **Additive, exactly as ADR-0001 promised.** No schema migration and no identity change:
  `file_revision` has no language column (the language lives inside the `parser_fingerprint`
  string), `parsed_unit.kind` is free-form `TEXT`, and the FTS materializer is kind-agnostic.
  Store, search pipeline, protocol, MCP surface, TUI and CLI need no change.
- **A bounded, measured re-index.** Claiming an extension changes those files' `lang=`, so they
  miss the structural-sharing pre-check on `UNIQUE (content_hash, parser_fingerprint)` and are
  re-parsed and re-embedded. Counted on the two enrolled worktrees: **29 files** on
  `local-rag-v2` (804 tracked) and **805** on `firefly` (14 147 tracked) — of which YAML alone is
  644. Following ADR-0012's precedent, the first cycle after this lands is a **measurement**, not
  a background event; group 24 spends a card on it.
- **`is_identifier_kind` gains node kinds, and that is the design working.** Go method names are
  `field_identifier`, Bash function names are `word`, TOML keys are `bare_key`, YAML keys are
  `flow_node`; none are in the shared allowlist today, so those declarations would silently
  degrade to ordinal anchors. Widening it is sanctioned by that module's own contract — the
  vocabulary "is a *superset* across the v0 languages: kinds/tokens that a given grammar never
  produces simply never match" — and the existing goldens are the proof that TS/JS/Rust did not
  move.
- **`is_safe_segment` will reject some real YAML keys**, e.g.
  `nginx.ingress.kubernetes.io/rewrite-target`, because it forbids `/` in a `syntax_path`
  segment. Those units correctly fall back to an ordinal anchor. The YAML card asserts this
  rather than weakening the `SyntaxLocator` invariant.
- **`references` stay inside the closed set.** Python's `import`/`from … import`, Go's
  `import_declaration` and Bash's `source`/`.` all map to `ReferenceKind::Import`; YAML and TOML
  emit none. No new variant is introduced, so `output.rs` and
  `fixtures/schema/parser.schema.json`'s `reference_kind` enum are untouched.
- **The CI guards from ADR-0001 do the enforcing.** `crates/index/tests/language_coverage.rs`
  asserts the set is identical across spec 02 §3.1, `Config::default`, and its own expectation,
  and that every non-corpus language is named in ADR-0001 — so a language cannot be added
  without amending ADR-0001 and the specification in the same change. `crates/xtask/tests/adr_links.rs`
  keeps this file's links honest.
- **Retrieval quality remains unmeasured for every language but TypeScript**, and this decision
  widens that gap from two languages to seven. The 49-query corpus is TypeScript-only; ADR-0001
  recorded the limitation for JavaScript and Rust and it now extends to the five added here.
  Their adapters are still held to the determinism and byte-span goldens of spec 14 §5.
- **Go has no files in either enrolled worktree** (0 of 804 and 0 of 14 147). Its card is
  validated by fixtures alone and cannot be confirmed by the live acceptance that covers the
  others. It is included as a forward-looking bet, and this sentence is here so that the absence
  of live evidence for Go is a recorded fact rather than an oversight.
