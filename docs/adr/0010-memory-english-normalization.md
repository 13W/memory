# ADR-0010: English normalization of durable memory

## Status

Accepted — 2026-08-18.

Wholly new scope, like ADR-0008 and unlike ADR-0009: no `[FIXED]` decision, specification
section, or plan card before this ADR says anything about what *language* durable memory is
stored or searched in. The question was raised by the owner, measured by `X-010`/`X-011`, and
deliberately left open by the measuring task itself.
[spec 08 §6](../specification/08-memory.md)'s own as-built note states the boundary verbatim:

> **This is evidence, not a resolved design decision** — no `[OPEN]` item in 15 §4 names this
> question, so there is nothing here to close silently, and building a real translation
> component is a separate, real architecture decision (which mechanism, whether it can satisfy
> `local_only`/no-mandatory-external-daemon, where in the pipeline it runs) that needs the
> owner's explicit sign-off, not an inference from a 24-query synthetic corpus.

This ADR records that sign-off and answers those three questions: **mechanism** — the local
generative model this project already ships (§Decision 2); **`local_only`** — satisfied by
construction, the translator is just another `GeneratorPool` consumer and the policy guard runs
before provider selection (§Decision 7); **where in the pipeline** — on the write path only,
behind the durable store, never on recall's hot path (§Decision 3, §Decision 9).

Delivery mechanism is **not** a single `X-NNN` card. `TASK-TEMPLATE.md`'s rule — "if the
description requires two independent results … the task must be split" — is violated many times
over: schema, effective-text plumbing, language detector, translator, write ordering, background
worker, privacy surfaces, observability and measurement are each separately testable. The owner
therefore chose the heavier path `TRACEABILITY.md` reserves for this case: a new numbered group,
`21`, gated by `G21`, outside the closed `T00–T17` v0 queue — the third instance of the
precedent ADR-0008 established. Group 21 reopens no gate `G00–G20`.

## Context

### What was measured

`X-010` built the first per-query relevance-judged benchmark for memory recall
(`cargo xtask memory-recall-bench`, corpus `fixtures/memory-recall/corpus.json`: 24 entry/query
pairs, 8 `ru-ru` / 8 `en-en` same-language controls, 4 `ru-en` / 4 `en-ru` cross-lingual).
`X-011` ran four configurations against the same corpus and the same real
`embeddinggemma-300m`:

| Group | `baseline` | `store_en` | `query_en` | `both_en` |
| --- | --- | --- | --- | --- |
| overall (n=24) | 0.8021 | 0.9792 | 0.9062 | 1.0000 |
| `en-en` (n=8) | 1.0000 | 0.9375 | 1.0000 | 1.0000 |
| `ru-ru` (n=8) | 1.0000 | 1.0000 | 0.9375 | 1.0000 |
| `ru-en` (n=4) | 0.5625 | 1.0000 | 0.5625 | 1.0000 |
| `en-ru` (n=4) | 0.2500 | 1.0000 | 1.0000 | 1.0000 |

The same-language groups were already perfect; the entire gap is cross-lingual, and normalizing
the **stored** text closes it completely (`ru-en` 0.5625 → 1.0000, `en-ru` 0.2500 → 1.0000).

### Correction to those numbers, found while planning this ADR

The `en-en` row of `store_en` (1.0000 → 0.9375) is an **artifact of a non-deterministic
benchmark**, not a property of one-sided normalization:

- `store_en` and `both_en` seed the store with `text_english` for **all 24 entries** — the two
  runs have byte-identical stores and differ only in query text;
- the single query where their ranks differ is `mrq-13`, an `en-en` pair, and for every `en-en`
  pair the fixture has `text_original == text_english` **and** `query_original == query_english`
  (verified). The two runs' inputs are byte-identical for that query;
- `crates/xtask/src/memory_recall_bench/run.rs:275-276` mints `memory_id` with `SystemUuidV7`
  while every entry shares `created_at = 1_000`, so spec 08 §6's documented final tie-break
  `(score desc, created_at desc, memory_id)` is decided by a random UUID whenever scores tie.

That half-point is also the entire `store_en` ↔ `both_en` overall delta (1.0000 − 0.9792 =
0.0208 = 0.5/24). Consequences for this decision, both recorded rather than smoothed over:

1. **The whole measured benefit comes from the store side.** The contribution of translating the
   *query* is statistically indistinguishable from zero on this corpus.
2. **The benchmark is not reproducible run to run.** Registered as `D-068`.

The `query_en` regression on `ru-ru` (1.0000 → 0.9375) is, by contrast, genuine: there an English
query really is matched against a Russian store.

### What exists today

- **No translation component of any kind.** `X-011` switched between `text_original` and
  `text_english` fixture fields (`run.rs:136-149`); the English side was hand-authored offline
  and no runtime translation call was ever made.
- **A local generative model is already required and already installed** for the memory router
  (ADR-0006, Gemma 4 E2B q4_0, 32768-token context, greedy-only decoding). It is reachable
  through `local_rag_embed::Generator`/`GeneratorPool`, and `local-rag init --download-models`
  installs it (D-045). Adding a second consumer costs no new dependency, no new weights, no
  daemon.
- **Memory text is embedded verbatim.** `crates/embed/src/backfill.rs:699` reads
  `SELECT memory_id, text FROM memory_entry` and embeds it as-is — unlike `content_blob`, which
  has a `normalized_text` step. There is no seam where a different text could be substituted.
- **The entry↔vector link is a computed hash, not a foreign key.**
  `subject_memory_entry(memory_id, text)` (`crates/core/src/identity/domain.rs:230`) is computed
  independently in three places: `crates/store/src/subjects.rs:270` (what backfill expects),
  `crates/embed/src/backfill.rs:703` (what gets embedded), `crates/memory/src/recall/dense.rs:211`
  (what recall looks up). Should those definitions ever disagree, the dense leg silently returns
  nothing for the affected entries. This is the single largest risk in the group and the reason
  §Decision 4 buys a structural guarantee rather than a convention.

### A live defect discovered while planning (`D-067`)

`dense_leg` reads cached vectors as
`embeddings_for_subject_kind(cache, MemoryEntry, representation_id, cap = candidates.len())`
(`crates/memory/src/recall/dense.rs:198-201`), while that reader returns rows for **every**
memory entry in the store — all scopes, all lifecycle states, plus stale hashes left by earlier
edits — under `ORDER BY subject_hash LIMIT ?3` (`crates/store/src/cache/embedding.rs:307-320`).
Whenever the cache holds more memory rows than the current scope union has candidates, the tail
is cut at an arbitrary point in hash order.

Measured on the owner's live store (read-only): 86 `memory_entry` rows in `embedding_cache`
against 44 non-terminal candidates for a recall in this repository — expectation ≈ 22 of 44
candidates resolve a vector; in a smaller repository (15 candidates) ≈ 17%. Nothing reports it:
`dense_degraded` is set only on an error, never on a silently missing vector.

This defect exists today, independent of normalization, and normalization makes it strictly
worse (a second row per translated entry). It is registered as `D-067` and must be resolved
before any card that adds rows to `embedding_cache`.

## Decision

**1. Normalize the stored text, not the stored record.** `memory_entry.text` remains canonical,
untouched, and is what every user-facing surface shows. The English variant lives in a new
`state.sqlite` table `memory_text_normalization` (schema v14) and feeds exactly two things: the
embedder's input and the subject hash. Rationale: spec 08 §3 `[FIXED]` allows a text change only
through `edit`'s new `entry_version` in the audit ledger, and `reinforce` may never change text
at all — a background translator writing into `memory_entry.text` would violate both. Keeping the
original also means recall never shows the user a machine translation of their own note.

**2. Mechanism: the local generative model already in the product.** No new model, no new
dependency, no external daemon, no network. The translator is a `GeneratorPool` consumer like the
router.

**3. Translation happens in the background, after the mutation commits.** `remember` and the
consolidation router keep their current latency; a translation costs ≈ 800 ms (p50, measured for
the router on the same model and machine) and must never sit inside a tool call.

**4. One definition of "which text is embedded", enforced structurally.** A new
`EffectiveText` type with a private field and no public constructor; the only function that can
hash a stored memory entry takes `&EffectiveText`; both readers obtain it from one shared
`LEFT JOIN` fragment and one shared row mapper; a source lint pins `subject_memory_entry` to
exactly two files. Reading the entry text and its normalization in a *single statement* also
makes "new text with old translation" unobservable — a property of the SQLite snapshot rather
than of developer discipline.

**5. Staleness is decided by `source_text_sha256`, never by `entry_version`.** `reinforce`
increments the version without touching the text (`crates/store/src/memory/op.rs:696-701`).
Every degraded case — no row, hash mismatch, `skipped`, `failed`, empty translation — falls back
to the original text, which is exactly today's hash, for which a vector already exists. Fallback
therefore returns the system to a known-good state rather than to a new one.

**6. Write order is vector first, normalization row second.** The worker embeds the English text
and inserts the vector under the new hash into `cache.sqlite`, and only then commits the
normalization row in `state.sqlite` (two separate transactions — spec 03 §1.4 forbids writable
cross-database transactions). The reverse order would leave a window in which an entry is already
normalized but has no vector under its new hash, dropping it from the dense leg entirely. A crash
between the two steps leaves an unreferenced cache row, which is harmless by the
`cache.sqlite`-is-rebuildable invariant and is reclaimed by ordinary eviction.

**7. Data policy is not re-implemented.** `GeneratorPool::generate` already enforces
`DataPolicy` before selecting a provider (`crates/embed/src/gen_pool.rs:154`), so spec 08 §4's
`[FIXED]` "under `local_only` the router runs on the local generator" holds for the translator
for free. The translator passes the effective policy and decides nothing itself.

**8. Already-English text costs zero inference.** A deterministic, model-free script detector
short-circuits to passthrough; passthrough leaves the effective text equal to the original, so
the subject hash does not change and `cache.sqlite` is not touched at all. An all-English store
converges in one tick at no cost.

**9. Query-side normalization is deferred and gated on measurement (`T21-10`).** Given the
correction above, the measured value of translating the query is zero; the honest sequence is to
ship the store side, fix the benchmark's determinism, measure the shipped component, and only
then decide. Should it proceed, it is constrained in advance: **recall must never call the
generator**, because `crates/local-rag-hook/src/recall.rs:89` budgets the entire hook exchange at
300 ms against an ≈ 800 ms inference, and `recall` runs synchronously on a tokio worker without
`spawn_blocking`. A query-translation cache in `cache.sqlite`, warmed by the same background
worker, is the only shape considered. Because the store side changes no pipeline stage, spec 08
§6's `[FIXED pipeline]` text is **not** amended by this group; only `T21-10` would need that.

**10. Failures degrade to today's behavior, never below it.** A missing or unusable generator
aborts the tick without marking any entry failed (the pre-emptive lesson of D-050, where a
deterministic failure was retried for hours); mechanical failures dead-letter per build
fingerprint; transient ones back off on the existing `transient_backoff_delay_ms` curve. Every
degraded path yields the original text — i.e. baseline quality.

**11. `normalize_to_english` defaults to `true`.** For an all-English store the feature costs
nothing (Decision 8); with no generator installed it is a no-op (Decision 10); the only case
where it does work is the case where it was measured to help. The owner can disable it in
`[memory]`.

*As built (T21-08):* `config.memory.normalize_to_english = true` and
`config.memory.normalization_batch = 4` (`crates/core/src/config`), read into the worker's
`NormalizationParams` at daemon start. Switching it off stops the work, not the reading — entries
already normalized keep being recalled through their English variant, whose vector is already
cached. `local-rag doctor` reports the switch's state, the backlog, and any dead-lettered entry;
`local-rag stats` reports the counts on both the CLI and the MCP surface.

*Amended (T21-11, 2026-08-20) — the default is now `false`.* Decision 11's argument was that the
only case where the worker does work is the case where it was measured to help. `T21-09` measured
the **shipped** component end to end and that premise did not hold: `pipeline_en` equals `baseline`
query for query (`Δ MRR = +0.0000`), because the translation feeds the *dense* leg and the dense leg
already ranked the expected entry #1 in 24/24 queries in every configuration, `baseline` included
(`D-075`). A default that spends ≈ 1 s of local GPU per entry for a measured zero is not a default,
so the switch ships off while the successor design lands.

Decision 3's "background, after the mutation commits" and Decision 1's "the stored record is never
rewritten" are superseded by ADR-0011 (English canon), on the owner's decision of 2026-08-20. The
first move of that design is the one this ADR never considered: **ask for English at the source.**
Nothing in the product ever told an agent which language durable memory is kept in —
`SERVER_INSTRUCTIONS` did not mention language, `remember.text` carried no description at all, and
the router's own few-shot set demonstrated mirroring the observation's language back into `text`.
Asking costs zero inference and acts on the source; translating costs a second of GPU per entry and
acts on the consequence. T21-11 changes all three, and translation stays only as a safety net for
what instructions cannot reach (verbatim quotes, other clients, small local models that do not
follow instructions reliably) and for entries written before it.

## Consequences

- Cross-lingual memory recall goes from MRR 0.25–0.56 to a measured 1.0 on the benchmark corpus;
  `T21-09` re-measures with the shipped component rather than the hand-authored ceiling, and a
  divergence there would prove the embedder is being fed the wrong text.
- A store with non-English memory pays ≈ 800 ms of local inference per entry, once, in the
  background, bounded per tick. Re-translation happens only when the text itself changes.
- **The detector distinguishes scripts, not languages.** Latin-script non-English text (German,
  French, Spanish, Polish, Turkish, …) is classified as English and therefore never normalized.
  Closing that would require either an n-gram language model (new dependency and weights in the
  distribution) or an LLM call on every entry and every query (which destroys Decision 8). Such
  text keeps today's behavior — it does not get worse. This limitation is stated in the detector's
  module documentation and surfaced in `local-rag doctor`, so it reads as a decision rather than
  as a bug found later.
- `D-067` is fixed as a precondition, which independently improves memory recall for every user
  of a multi-project store, feature enabled or not.
- `D-068` makes the recall benchmark byte-reproducible, which every future memory-quality
  measurement depends on.
- `local-rag doctor` gains a generative-model check it never had — until now a missing generative
  model surfaced only as consolidation runs silently failing with `NoProvider`.
- Group 21 introduces schema v14. Migration is inert on upgrade: an empty table means every
  effective text is the original, every subject hash is unchanged, and every existing vector stays
  valid.

## Alternatives rejected

- **Store the translation in `cache.sqlite`.** It is not locally recomputable — restoring it needs
  an LLM that may be unavailable — which contradicts the "fully rebuildable cache" invariant. It
  would also make the cache define what is expected of itself, since `expected_subject_keys` would
  have to read it.
- **Add columns to `memory_entry`.** `SCHEMA_V9` is frozen by checksum; the precedent for a derived
  axis is a separate table (X-006's `worktree_indexing_status`). A separate table also gets
  `ON DELETE CASCADE` and a countable, explicit purge for free.
- **Persist the subject hash in a column.** It would require writing a row at `create` time — that
  is, making `remember` more expensive, which Decision 3 rules out — and still leaves the fallback
  logic. It also contradicts the project's convention that subject sets are computed, not stored.
- **Compute the hash in SQL.** Impossible without registering a `sha256()` UDF on every connection,
  which would add a third definition of "the effective text" instead of removing the second.
- **Index both the original and the translation in recall's ephemeral FTS table.** During partial
  conversion, translated entries would have documents twice as long, biasing BM25 by exactly the
  "already translated" property. The lexical leg stays raw-against-raw, which is monolingual by
  nature and preserves baseline behavior byte for byte; only the dense leg becomes cross-lingual.
- **Translate the query inline with a deadline.** Under a 300 ms hook budget an ≈ 800 ms inference
  times out nearly always — maximum cost for zero benefit.
- **Chunked translation of very long entries.** Requires segmentation, cross-chunk consistency and
  reassembly, and would serve entries that recall already caps at 1 KiB when rendering. Entries
  above the input limit are dead-lettered with an explicit reason instead.
