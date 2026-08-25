# ADR-0012: The universal file-indexing path

## Status

Accepted — 2026-08-25.

Delivered by deviation **D-098**
([group 21 cards](../implementation-plan/groups/21-memory-english-normalization.md)). Closes the
dangling pointer left by [ADR-0001](0001-first-release-language-set.md)'s "Scope boundary" section,
which is amended here rather than superseded: its language *set* stands unchanged.

## Context

[Spec 06 §2.1](../specification/06-reconcile-and-fts.md) has always declared, `[FIXED]`:

> Unit kinds: `symbol | file | config_section | text_section | fallback_chunk` — **all kinds are
> indexed** (v1 parity requirement).

Three of those five had no producer. ADR-0001's own scope boundary said where they would come
from — "they are **not** part of the O4 language set and are specified later (T04-06 / groups 05
and 08)" — and every one of those addresses turned out to be wrong:

- **T04-06** is "Deterministic parsed-unit persistence"; it never touched chunking.
- **Group 05** deferred it explicitly and closed `PASS`, recording the deferral as by-design in
  `PROGRESS.md`.
- **Group 08** made the FTS materializer kind-agnostic and tested it against all five kinds — a
  consumer with no producer — and closed `PASS`.

So a `[FIXED]` requirement had no owner in any card, and the deferral was invisible by
construction: `crates/index/src/reconcile/build.rs` counted such a file as `files_deferred` and
wrote it **nowhere** — not into `generation_file`, not into `skipped_file`, not into any report.

What that cost, measured on the owner's store before this decision (`D-096` made the measurement
possible; `local-rag project coverage` is the command):

| Worktree | Files in the tree | Accounted for | In neither table |
| --- | --- | --- | --- |
| `/opt/legatics.com/firefly` | 13 748 | 10 293 | **3 455** |
| `/opt/soft/local-rag-v2` | 739 | 493 | **246** |

Not one of the missing files had a supported extension. The firefly figure is a quarter of the
repository: 584 `.yaml`, 560 `.gql`, 384 `.svg`, 216 `.md`, 216 `.json`, 190 `.graphql`, 97 `.sh`,
49 `.py`, 42 `.tf`. On this repository the 246 include **all 119 `.md` files**, so the product's
own specification, its `idea.md`, its implementation plan and every ADR — this one included — were
absent from its own code search.

## Decision

### 1. The universal path is a sibling of the language set, not a member of it

ADR-0001's v0 language set stays exactly `{TypeScript, JavaScript, Rust}` and `LanguageId` stays a
closed enum. The universal path introduces `UniversalKind = {Config, Text, Fallback}` and a
`SourceDialect = Language(LanguageId) | Universal(UniversalKind)` that occupies the single `lang=`
field of a `parser_fingerprint` and a `SyntaxLocator`.

They share a field because they *are* one field. They are not peers in meaning: a `LanguageId`
names a tree-sitter grammar; a `UniversalKind` names a chunking policy. Adding three more
`LanguageId` variants would have made ADR-0001 false in order to make spec 06 §2.1 true.

### 2. Extension → policy

Extension-only and case-insensitive — the same rule `select_language` applies, so the two selectors
cannot disagree about what a path is.

| Policy | Extensions | Unit kind |
| --- | --- | --- |
| `Config` | `yaml` `yml` `json` `jsonc` `json5` `toml` `ini` `cfg` `conf` `properties` `env` `tf` `tfvars` `hcl` `tfstate` | `config_section` |
| `Text` | `md` `mdx` `markdown` `txt` `rst` `adoc` `asciidoc` | `text_section` |
| `Fallback` | everything else, including no extension at all | `fallback_chunk` |

`Fallback` is the default rather than a listed set, and that totality is the decision. A selector
that can answer "none" reintroduces exactly the state this ADR exists to remove.

### 3. Refusal is explicit, and `binary` means "not source text"

Because every accepted file is now chunked, content that should *not* be indexed has to be refused
on purpose. `svg`, `ipynb`, `drawio` and `map` join the built-in `BINARY_EXTENSIONS`: each is
textual (XML or JSON, so no NUL sniff will ever catch it) and each is a serialized artifact — a
picture, a notebook's stored outputs, a diagram, a source map — whose bytes are machine-written.

This widens the meaning of spec 06 §2.2's `binary` reason from "contains a NUL byte" to "content
that is not source text". The alternative — a seventh `skipped_file.reason` — was rejected: it
needs a schema migration to carry a distinction no consumer acts on, and `skipped_file`'s primary
key already admits exactly one reason per path.

### 4. Chunking is dependency-free, and that is a requirement, not thrift

No YAML, JSON or Markdown parser is linked. The obvious reason is the T10 dependency guardrail. The
load-bearing reason is [spec 03 §2.3.1](../specification/03-data-model.md): spans must address the
exact `source_blob`. A real parser hands back a value tree, not byte offsets into the text it
consumed; a chunker built on one cannot produce exact spans without re-deriving them. Line-oriented
scanning produces them directly.

- **Text** — sections are ATX headings (`#`…`######` followed by whitespace, which is what separates
  a heading from a `#!` shebang or a `#region` marker). A section's name is the **heading trail**
  (`Install/From source`), so a nested heading is never confused with a top-level one of the same
  text. Content before the first heading is an unnamed preamble section.
- **Config** — sections are top-level keys, where "top level" is the **minimum indentation among
  the file's own key lines**, not column zero. That single rule serves YAML (keys at column 0),
  pretty-printed JSON (keys at column 2 inside the root object) and INI/`.env` alike: the file
  states its own top level by where its keys sit.
- **Fallback** — line-aligned windows of at most `MAX_SECTION_BYTES = 2048`.

Every file also yields a `file` unit spanning the whole content, the same shape the tree-sitter
adapters produce, so an accepted file can never be indexed with nothing searchable in it. A section
larger than the cap is split on line boundaries; a single line larger than the cap is its own
section, because splitting mid-line would put a span boundary inside a token for no benefit.

Anchors are path-free per [ADR-0002](0002-syntax-locator-derivation.md): a heading trail or a key
for named sections, a `LocalOrdinal` for fallback windows — which is honest about being positional.
A repeated name becomes `Name#2`, so two sections never share a locator anchor.

### 5. The fingerprint format is unchanged

`chunk=1;grammar=universal@1;lang=<config|text|fallback>;norm=1;queries=0` — the same five keys,
ASCII-sorted, as spec 03 §2.3.1 `[SPEC]` fixes. `grammar` holds `universal` because it is the slot
the format provides; semantically `UNIVERSAL_POLICY_VERSION` versions a chunking policy, and bumping
it is a rebuild event on exactly the same terms as a grammar bump. `queries=0` because the universal
path runs no tree-sitter query set at all, which is more honest than borrowing `1` from a query file
that does not exist.

## Consequences

- **The invariant becomes checkable.** Every file the scan produced is in `generation_file` **or**
  `skipped_file`. `BuildOutcome::files_deferred` is deleted along with the third outcome it counted,
  and `crates/index/tests/reconcile.rs::every_scanned_file_is_either_indexed_or_skipped` asserts the
  tiling directly. `local-rag project coverage` (D-096) reports `0`.
- **No migration.** `parsed_unit.unit_kind` already `CHECK`s all five kinds; `file_revision` has no
  language column (the language lives inside the `parser_fingerprint` string); the FTS materializer
  is already kind-agnostic and was already tested against kinds nothing produced.
- **Cost is real and must be measured, not assumed.** Roughly 2 300 additional files enter firefly's
  index, with their units and embeddings. That is the same resource `D-083`/`D-086`/`D-089` were
  fought over, so the first cycle after this lands is a measurement, not a background event.
- **Python is not a language.** Firefly's 49 `.py` files are `fallback_chunk`s. A real tree-sitter
  Python adapter is a new ADR amending ADR-0001's set, not part of this one.
- **Adding a language later stays additive**, exactly as ADR-0001 promised: a new `LanguageId`
  variant moves files from `Fallback` to a grammar, changing their `parser_fingerprint` and hence
  rebuilding them, with no schema or identity change.
