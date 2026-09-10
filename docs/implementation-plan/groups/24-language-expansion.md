# Group 24 — Post-v0 language expansion (Python, Go, Bash, YAML, TOML)

The seventh group opened after `T00–T17` closed (`G17: PASS`), by the owner's explicit product
decision of 2026-09-04. The decision itself is recorded in
`docs/adr/0015-post-v0-language-expansion.md`, written by card `T24-00` together with the
amendment it makes to `ADR-0001`. No gate `G00–G23` is reopened.

Goal: the file types a real repository is actually made of get units with names. `.py`, `.go` and
`.sh` stop being anonymous 2048-byte windows; `.yaml` and `.toml` stop being named by a line
scanner that cannot see a TOML table header or a YAML document boundary.

References: `ADR-0015` (created by `T24-00`); `ADR-0001` (the v0 set this amends); `ADR-0002`
(the shared engine and locator derivation); `ADR-0012` (the universal path these extensions
leave, and the precedent that a re-index is measured rather than assumed); spec 02 §3.1
(`index.languages`); spec 03 §2.3.1 (`parser_fingerprint`) and §2.4 (`SyntaxLocator`); spec 06
§2.1 (parsing rules, the `[FIXED]` unit-kind set); spec 14 §1/§5 (fixtures and determinism);
`crates/index/src/parse/adapter/`; `crates/index/src/parse/language.rs`;
`crates/index/src/parse/fingerprint.rs`; `crates/index/src/parse/universal/mod.rs`;
`crates/index/tests/{parse_fixtures,parse_identity,language_coverage,reconcile}.rs`;
`fixtures/parser/`; `CONTRIBUTING.md` (the dependency allowlist); `D-096` (`project coverage`,
the tool the live acceptance uses).

Card format follows `TASK-TEMPLATE.md`; group structure follows groups 18/20/21/22/23. One task —
one iteration — one commit.

## What was measured before the group was opened

Recorded here so a later reader can tell what this group was looking at, and so nobody re-derives
it. All of it is reproducible.

1. **Five grammars load on the pinned core; one does not.** A throwaway crate built against the
   project's exact `tree-sitter 0.24.7` (`LANGUAGE_VERSION = 14`, `MIN_COMPATIBLE = 13`) loaded
   and parsed `tree-sitter-python 0.23.6`, `tree-sitter-go 0.23.4`, `tree-sitter-bash 0.23.3`,
   `tree-sitter-yaml 0.7.2` and `tree-sitter-toml-ng 0.7.0` — every one ABI 14, MIT, and adding
   **zero new packages** to the lockfile. `tree-sitter-hcl 1.1.0` is ABI 15 and was refused:
   `Incompatible language version 15. Expected minimum 13, maximum 14`. Terraform is therefore
   out of this group, and `ADR-0015` §5 records why.

2. **The line scanner misnames two of the three config formats it serves.** Running the real
   `config_key` (`crates/index/src/parse/universal/mod.rs`) over samples:

   ```
   TOML       sections: name, version, serde, name
              [package] / [dependencies] / [[bin]] are never section starts
   k8s YAML   sections: apiVersion, kind, metadata, spec, apiVersion, kind
              documents separated by --- are merged into one flat key list
   ```

3. **The queries are already known to compile and capture correctly.** Candidate `.scm` files
   were compiled against each real grammar and run over samples before this group was written:
   Python 8 captures, Go 6, Bash 5 (with `echo`/`ls`/`grep` correctly excluded by
   `(#match? @_cmd "^(source|\\.)$")`), YAML 5 with **both documents split**, TOML capturing
   exactly `[package]`, `[dependencies]` and `[[bin]]`. Standard text predicates (`#match?`,
   `#eq?`) **are** applied by the Rust binding; only *general* predicates (`#strip!`,
   `#select-adjacent!`, the doc-comment machinery of upstream `tags.scm`) are not.

4. **The re-index is bounded, and Go is invisible.** `git ls-files` against the two enrolled
   worktrees: `local-rag-v2` 804 tracked → **29 affected** (24 `toml`, 3 `yaml`/`yml`, 1 `sh`,
   1 `py`, **0 `go`**); `firefly` 14 147 tracked → **805 affected** (644 `yaml`/`yml`, 107 `sh`,
   49 `py`, 5 `toml`, **0 `go`**). YAML is 80 % of the total. Go has no live evidence at all,
   which is why its card is validated by fixtures alone and says so.

5. **Two shared vocabularies are too narrow.** Go method names are `field_identifier`, Bash
   function names are `word`, TOML keys are `bare_key`, YAML keys are `flow_node` — none are in
   `is_identifier_kind` (`crates/index/src/parse/adapter/mod.rs`), so without widening it those
   declarations degrade to ordinal anchors silently.

   *Corrected by measurement in `T24-02`:* this survey missed Bash **declaration** names, which
   are `variable_name` (`local x=1` is `declaration_command → variable_assignment → name:
   variable_name`), not `word`. `T24-02` adds that token, so the widening happened in two steps
   rather than the one `T24-01` planned. A later card should measure its own name nodes rather
   than trust the list above.

## The shape every language card shares

Stated once here; each card below names only what is specific to it.

- **New files:** `crates/index/src/parse/adapter/<lang>.rs` (the shape of `adapter/rust.rs`:
  struct, `new()`, `Default`, `impl LanguageParser`, `impl LanguageSpec`, private helpers,
  `#[cfg(test)] mod tests`) and `crates/index/src/parse/adapter/<lang>.scm` with a header comment
  declaring `query_version = 1`.
- **The compiler is the checklist** for `LanguageId::ALL` (its array length is hard-coded),
  `as_str`, `fingerprint::descriptor`, `parser::parser_for`, the module declaration and the
  re-export.
- **The compiler is silent** about `from_str_value` and `select_language`'s extension table.
  Forgetting the second wires up a language that is never selected.
- **Set consistency moves as one step:** the `LanguageId` variant, `Config::default().index.languages`,
  the `languages = [...]` array in spec 02 §3.1, and `expected_set()` in `language_coverage.rs`
  change in the same commit. `parse_identity.rs` and `language_coverage.rs` assert they are
  identical, so a card that moves one and not the others fails.
- **Dependency policy:** the pin in `crates/index/Cargo.toml` carries the same ABI-14 comment the
  three existing grammars carry, and `CONTRIBUTING.md` gets a new allowlist row.
- **Fixtures:** ≥ 4 cases in `fixtures/parser/index.json` covering `syntax`, `error`, `empty` and
  `unicode`; the closed `language` enum in `fixtures/schema/parser.schema.json` widened; the
  per-language Status section in `fixtures/parser/README.md`.
- **Goldens:** the `rust.rs` test template, including `grammar_loads_and_extracts_symbols` (the
  ABI tripwire), `byte_spans_are_exact`, `parse_is_deterministic`, `unicode_spans_are_byte_offsets`
  and a fresh `locator_and_signature_goldens` pinning a literal 64-hex `sig`.
- **Hard-coded lists that fail at run time, not compile time:** the exact fingerprint goldens and
  `version_constants_and_descriptors_are_pinned` in `fingerprint.rs`; the expectations in
  `parse_identity.rs`, `language_coverage.rs`, `parse_fixtures.rs` and `reconcile.rs`.
- **No new `UnitKind`** (`parsed_unit.unit_kind`'s `CHECK` admits exactly five) and **no new
  `ReferenceKind`** (`Import | TypeImport | Reexport` is closed).

## T24-00 — Register the group and record the decision

- **Depends on:** `G23`.
- **Specification:** spec 02 §3.1; spec 03 §2.3.1; spec 15 §2/§4; `ADR-0001`; `ADR-0012`;
  `TRACEABILITY.md` §"Новая scope".
- **Result:** `ADR-0015` exists, `ADR-0001` is amended in place, the two `[SPEC]` sections carry
  as-built notes, and this group is registered in `PROGRESS.md`.
- **In scope:** the ADR and its amendment; the `[SPEC]` notes in spec 02 §3.1 and spec 03 §2.3.1;
  this group file; the `PROGRESS.md` section, task list and `G24` gate row.
- **Not in scope:** any code, any dependency, any change to the `languages = [...]` array — the
  set grows per language card, together with the code, or the consistency tests fail.
- **Tests:** `cargo test -p xtask --test adr_links` (the new ADR's relative links resolve, and
  `ADR-0001` stays well-formed); `cargo test -p local-rag-index --test language_coverage
  --test parse_identity` — both must stay green **unchanged**, proving the documentation moved
  without the set moving.
- **Acceptance:** the normative route ADR-0012 named is taken and closed; the `[FIXED]` "2–3
  languages" question of `idea.md` §16 is answered in writing rather than left to the next reader.
- **Evidence:** the `T24-00` row.

## T24-01 — Python adapter

- **Depends on:** `T24-00`.
- **Specification:** spec 03 §2.3.1/§2.4; spec 06 §2.1; spec 14 §1/§5; `ADR-0002`; `ADR-0015`.
- **Result:** `.py`/`.pyi` files produce named `symbol` units instead of anonymous fallback windows.
- **In scope:** `tree-sitter-python 0.23`; captures for `function_definition`, `class_definition`,
  `decorated_definition` and module-level `assignment`; `import_statement`/`import_from_statement`
  as `ReferenceKind::Import`. **Also the shared-vocabulary widening for the whole group**, done
  once here: `is_identifier_kind` gains `field_identifier`, `word`, `bare_key`, `flow_node`,
  `dotted_name` and `package_identifier`.
- **Not in scope:** docstring extraction — no mechanism for doc comments exists anywhere in the
  parse output, and adding one is a cross-cutting change with a `chunk=`/`queries=` rebuild for
  every language.
- **Tests:** the shared template; plus a case pinning the `decorated_definition` decision — a
  decorated method must yield exactly one unit with a defined owner of the span, not two
  overlapping ones; plus a regression proving the widened `is_identifier_kind` left the
  TypeScript, JavaScript and Rust goldens byte-identical.
- **Acceptance:** `search_code` finds a Python function by name on a fixture store.
- **Evidence:** the `T24-01` row.

## T24-02 — Bash adapter

- **Depends on:** `T24-01`.
- **Specification:** as `T24-01`.
- **Result:** `.sh`/`.bash` files produce named `symbol` units.
- **In scope:** `tree-sitter-bash 0.23`; captures for `function_definition` (both the `name()` and
  `function name` forms) and `declaration_command`; `source`/`.` as `ReferenceKind::Import`,
  filtered by `(#match? @_cmd "^(source|\\.)$")`.
- **Not in scope:** upstream ships no `tags.scm` for bash, so the query is hand-authored; do not
  invent one from another grammar's vocabulary.
- **Tests:** the shared template; both function-definition syntaxes yield a named unit (the name
  node is a `word`, which is the reason `T24-01` widened the allowlist); `echo`/`ls`/`grep` do
  **not** produce references.
- **Acceptance:** a shell function is findable by name on a fixture store.
- **Evidence:** the `T24-02` row.
- **As built:** the declaration rows are **not** restricted by parent, unlike `python.scm`'s.
  A `function_definition` has sixteen possible parents in `grammar.json`, so enumerating them
  would be a transcription with no benefit — parents come from span containment in the engine,
  not from the query. Matching at any depth is also what makes a definition recovered inside an
  `ERROR` region keep its name, which for bash is **observed**, not insurance.

## T24-03 — Go adapter

- **Depends on:** `T24-02`.
- **Specification:** as `T24-01`.
- **Result:** `.go` files produce named `symbol` units.
- **In scope:** `tree-sitter-go 0.23`; captures for `function_declaration`, `method_declaration`,
  `type_declaration`, `const_declaration`, `var_declaration`; `import_declaration` as
  `ReferenceKind::Import`.
- **Not in scope:** a live acceptance. **Neither enrolled worktree contains a single `.go` file**,
  so this card is validated by fixtures and goldens alone; the card must say so in its evidence
  rather than quietly reporting an unmeasured success.
- **Tests:** the shared template; a `method_declaration` yields its `field_identifier` name (not
  an ordinal anchor) — this is the specific regression the widened allowlist exists for.
- **Also in scope — `D-134`.** `non_corpus_languages_are_acknowledged_in_adr`
  (`crates/index/tests/language_coverage.rs`) matches each language name against ADR-0001 by
  lowercase **substring**. `"go"` is a substring of `goldens`, `going` and `algorithm`, all of
  which ADR-0001 already contains, so the assertion becomes **vacuous the moment `go` joins the
  set** — a test that claims to enforce the ADR acknowledgment and silently stops. Fix it in this
  card (word-boundary match, or match the backticked canonical token) and resolve the row.
- **Acceptance:** fixtures and goldens green; no live figure claimed. `D-134` resolved, with the
  corrected assertion demonstrated to fail on an ADR-0001 that omits a language name.
- **Evidence:** the `T24-03` row, stating explicitly that live evidence is absent and why.

## T24-04 — TOML adapter

- **Depends on:** `T24-03`.
- **Specification:** as `T24-01`, plus `ADR-0015` Decision 2 and 3.
- **Result:** `.toml` files are chunked by a grammar, and a `[table]` header is finally a unit.
- **In scope:** `tree-sitter-toml-ng 0.7`; captures for `table`, `table_array_element` and
  top-level `pair`, all emitting **`UnitKind::ConfigSection`**, named by `bare_key`; removal of
  `toml` from `CONFIG_EXTENSIONS` in the same commit, plus the group's test that no extension
  appears in both selectors.
- **Also in scope — the parent seam, because this is the card that first needs it.** `finalize`
  admits only `UnitKind::Symbol` as a parent (`adapter/mod.rs`), so `config_section` units come out
  flat and `table:dependencies/key:serde` is unreachable. Open it as a defaulted `LanguageSpec`
  method (`parent_unit_kinds`, in the style of `file_lang_kind`/`fallback_lang_kind`) rather than
  by widening the predicate in place. It has no observable result of its own, which is why it
  lives in the card that consumes it rather than in one of its own — and its inertness is proved
  by the three existing signature goldens and the 25 existing fixtures staying byte-identical.
- **Also in scope — the `[SPEC]` amendment to spec 06 §2.1.** "Chunking takes on **no dependency**
  — no YAML, JSON or Markdown parser" and "One rule then serves YAML (column 0) …" both stop being
  true once TOML and YAML leave the universal path. Scope the sentences to the *universal chunker*
  and re-exemplify the Config rule with JSON/INI. `ADR-0015` Decision 3 carries the reasoning: the
  paragraph's own justification (a real parser returns a value tree, not byte offsets) is exactly
  what does not apply to tree-sitter.
- **Not in scope:** changing `config_key`'s behavior for the extensions that stay on the universal
  path.
- **Tests:** the shared template; a `Cargo.toml` fixture yields sections named `package`,
  `dependencies` and `bin` — the exact case the line scanner gets wrong today; and the
  both-selectors test.
- **Acceptance:** the misnaming recorded in "What was measured" §2 is gone, demonstrated by the
  fixture.
- **Evidence:** the `T24-04` row.

## T24-05 — YAML adapter

- **Depends on:** `T24-04`.
- **Specification:** as `T24-04`.
- **Result:** `.yaml`/`.yml` files are chunked by a grammar, and a multi-document file stops
  merging its documents.
- **In scope:** `tree-sitter-yaml 0.7`; captures for `document` and the document's own top-level
  `block_mapping_pair`, emitting **`UnitKind::ConfigSection`**; removal of `yaml`/`yml` from
  `CONFIG_EXTENSIONS`.
- **Not in scope:** schema-aware naming (`Deployment/my-app`). That is a naming rule over this
  adapter's output, deliberately a separate change — `ADR-0015` §5 records why it is not a grammar
  question.
- **Also in scope — two tests that go silently vacuous.** `crates/index/tests/reconcile.rs` writes
  `conf/values.yaml  // config` under the comment "One file per route the builder can take", and
  `deploy/values.yaml  // universal: config` in a second test. Once `.yaml` takes the *language*
  route, both keep passing while covering nothing: the config route stops being exercised and the
  comments become false. Switch those fixtures to an extension that stays universal (`.ini`), or
  add one alongside. A test that passes for the wrong reason is worse than one that fails.
- **Tests:** the shared template; a three-document manifest yields three document units, not one
  flat key list; the query is anchored so a nested key (`name: app`) does **not** become a
  top-level section; and a key containing `/` (e.g. `nginx.ingress.kubernetes.io/rewrite-target`)
  correctly falls back to an ordinal anchor rather than weakening `is_safe_segment`.
- **Acceptance:** the misnaming recorded in "What was measured" §2 is gone for YAML.
- **Evidence:** the `T24-05` row.

## T24-06 — Live acceptance: the re-index is measured, not assumed

- **Depends on:** `T24-05`.
- **Specification:** `ADR-0012` Consequences ("cost is real and must be measured"); `ADR-0015`
  Consequences; spec 06 §2.
- **Result:** the actual cost and benefit of this group on the owner's two stores, recorded.
- **In scope:** `local-rag project coverage --json` for both worktrees before and after; the
  rebuild and daemon restart; unit-count deltas; spot-checks that names are what the fixtures
  promised (`[package]` for TOML, per-document units for YAML, function names for Python and Bash).
- **Not in scope:** fixing anything found — a finding becomes a `D-NNN` under the normal workflow.
- **Tests:** none new; this card is a measurement.
- **Acceptance:** `unaccounted` stays `0` on both worktrees; the number of re-parsed files is
  within reach of the predicted 29 and 805, and any gap is explained rather than rounded away.
- **Evidence:** the `T24-06` row, with before/after figures from both live stores.

## G24 — Language expansion gate

- **Depends on:** `T24-06`.
- **Result:** `PASS`, `PASS after D-NNN`, or `BLOCKED`.
- **In scope:** reread spec 02 §3.1, spec 03 §2.3.1/§2.4, spec 06 §2.1 and spec 14 §1/§5; build a
  `requirement → code → test` trace for all five languages; inspect every `[FIXED]`/`[SPEC]`
  touched; confirm `ADR-0001` and `ADR-0015` still describe the as-built set exactly.
- **Tests:** the full group set plus `cargo xtask ci`.
- **Acceptance:** recorded in the Gate results table with reproducible evidence.
- **Evidence:** the `G24` row.
