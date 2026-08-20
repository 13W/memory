# ADR-0011: English canon for durable memory

## Status

Accepted — 2026-08-20.

**Supersedes [ADR-0010](0010-memory-english-normalization.md) Decisions 1, 3 and 6.** This is the
first ADR in this repository to supersede another, so it also settles how that is recorded: a
supersede names the *decisions* it replaces, not the whole record, and the superseded ADR keeps
its text and gains a pointer here. An ADR is evidence of what was decided and why — including
what turned out to be wrong — so nothing in ADR-0010 is rewritten or deleted.

Decision by decision:

| ADR-0010 | Fate |
| --- | --- |
| 1. Normalize the stored text, not the stored record | **Superseded** — `memory_entry.text` becomes the English text |
| 2. Mechanism: the local generative model already shipped | In force |
| 3. Translation happens in the background, after the mutation commits | **Superseded** — normalization happens at the boundary, before the write |
| 4. One definition of "which text is embedded", enforced structurally | **Moot** — an entry has one text again, so there is nothing to disambiguate |
| 5. Staleness decided by `source_text_sha256`, never `entry_version` | **Moot** — same reason |
| 6. Write order is vector first, normalization row second | **Superseded** — an entry is born English, so its first vector is its only vector |
| 7. Data policy is not re-implemented | In force |
| 8. Already-English text costs zero inference | In force, and load-bearing (§Decision 8) |
| 9. Query-side normalization deferred and gated on measurement | **Answered** — it happens (§Decision 2); the measurement that gated it is `D-075` |
| 10. Failures degrade to today's behavior, never below it | In force, sharpened by §Decision 3 |
| 11. `normalize_to_english` defaults to `true` | Already amended to `false` by `T21-11` |

`[FIXED]` text amended under this ADR: [08 §3](../specification/08-memory.md) (which actor may
change an entry's text, and the language invariant), [08 §6](../specification/08-memory.md)
(recall gains a query-normalization stage), [09 §1](../specification/09-search.md) (the same
stage in `search_code`). Each amendment points back here.

Delivered by group 21 phase 2 (`T21-12`…`T21-19`),
[groups/21](../implementation-plan/groups/21-memory-english-normalization.md).

## Context

### What phase 1 built, and what it had actually measured

ADR-0010 rested on `X-011`'s `store_en` configuration, which closed the cross-lingual gap
completely (`ru-en` 0.5625 → 1.0000, `en-ru` 0.2500 → 1.0000). But `store_en` seeds the store by
replacing `memory_entry.text` outright (`crates/xtask/src/memory_recall_bench/run.rs:197-198`),
so it feeds English to **both** recall legs — BM25 and the embedder alike.

ADR-0010 Decisions 1 and 6 then deliberately kept `memory_entry.text` canonical and BM25
raw-against-raw. The shipped component therefore changed **one** leg, while the measurement that
justified it had changed **two**. That gap was visible in the harness at planning time and was
never stated.

`T21-09` measured the shipped component end to end: `pipeline_en` equals `baseline` query for
query, `Δ MRR = +0.0000`, with 12 successful translations and zero failures. Registered as
`D-075`.

### What that measurement can and cannot support

The instrumented run also showed the dense leg ranking the expected entry **#1 in 24/24 queries
in every one of the five configurations, `baseline` included**. That is the real finding, and it
cuts both ways:

- it explains the zero — the leg that got the translation was already perfect, so no translation
  could improve it;
- it also means the corpus is **saturated on the dense leg**. Twenty-four entries of ~200
  characters, one expected entry per query, mutually distinct topics: retrieving one of
  twenty-four is not a task a 768-dimensional multilingual embedder can fail. Such a corpus can
  neither prove nor disprove an embedding-side effect at realistic scale.

So both the decision to build phase 1 (`X-011`) and the finding that killed it (`T21-09`) rest on
the same twenty-four documents. This ADR does not claim an MRR improvement, and must not be read
as predicting one: `T21-18` exists to build a corpus that can answer the question, and the honest
position until it runs is that the retrieval effect is unmeasured, not proven.

**What justifies this ADR is not a metric.** It is the owner's product decision that durable
memory is kept in one language, and the structural argument that a store whose two retrieval legs
read text in different languages cannot rank coherently — which is exactly the defect phase 1
shipped.

### The cheapest lever was never pulled

Nothing in the product had ever asked for English. `SERVER_INSTRUCTIONS` described the entire
working loop without naming a language; `remember.text` — the one field whose content *becomes*
the memory — carried no description at all; and the consolidation router's own few-shot set
demonstrated mirroring an observation's language back into `text`. A subsystem was built to
repair the consequence before the source was ever addressed.

`T21-11` fixed that first, at zero inference cost, and turned the phase-1 worker off.

## Decision

**1. English is the canon.** `memory_entry.text` stores English. The text as the author wrote it
is preserved as provenance beside the entry and is what `inspect`/`export` show the owner
(§Decision 4). This supersedes ADR-0010 Decision 1, whose reasoning — that spec 08 §3 `[FIXED]`
permits a text change only through `edit`'s audited `entry_version` — is satisfied rather than
violated: a normalization rewrite *is* such an edit, performed by `Actor::System`, and 08 §3 is
amended only to name that third actor alongside user and router.

**2. Normalization happens at the boundary — both boundaries.** Incoming text is script-detected
and, if it is not English, translated, before the service does anything else with it:

- the **write** boundary — `remember` and the consolidation router's materialization path, above
  the store (`crates/store` gains no generator: a model must not run under the write lock, the
  precedent D-063 set for subprocesses);
- the **query** boundary — recall and `search_code` alike, so both legs of both pillars match
  text in one language against a query in that same language.

This supersedes ADR-0010 Decision 3 ("background, after the mutation commits") and answers its
Decision 9. The latency it costs is accepted knowingly and bounded by §Decision 8.

**3. "Eventually English", not "always English".** If translation fails, the entry is stored with
the author's text, queued, and its canon is rewritten later by `Actor::System` with an audit
record. Losing somebody's note because a local model produced malformed JSON is not an acceptable
failure mode, and refusing the write would be exactly that. This sharpens ADR-0010 Decision 10:
degradation still never goes below the pre-normalization behaviour.

**4. The original is provenance, not loss.** It is stored, and it stays visible in `inspect` and
`export` (spec 12 §3). Two reasons, and the second is the load-bearing one: the owner must be able
to read what they actually wrote, and a translation is an inference — keeping the input means a
later, better normalizer can redo the work instead of compounding the error.

**5. Scope boundary — `code_raw` stays verbatim.** Indexed source is never translated. The lexical
leg exists to match identifiers exactly; translating code would destroy the one thing it is good
at. Only *queries* against it are normalized (§Decision 2).

**6. Scope boundary — code descriptions are English.** The description leg
(`structural_description`, `semantic` mode) is deferred post-v0 scope, so this is a rule for
whoever builds it rather than work in this wave: descriptions are generated and stored in English,
and the generator's prompt asks for English the same way the router's prompt now does
(`crates/memory/src/prompt.rs`, `T21-11`). Recording it now is the point — the rule is cheap to
honour while the pillar is being built and expensive to retrofit afterwards, which is the lesson
phase 1 paid for.

**7. Scope boundary — observations and evidence stay verbatim.** What hooks capture and what
`give_feedback` records is a record of what was actually said. Translating it would turn evidence
into paraphrase and hollow out `inspect_memory_evidence`. There is no inconsistency with
§Decision 1: the router writes `text` in English regardless of the language of the observations it
read, so the durable entry is English anyway. The rule applies where it has a purpose — memory and
search — and not where verbatimness *is* the purpose.

**8. Ask before translating.** Asking for English in the server instructions, the tool
descriptions and the router prompt costs no inference and acts on the source; translating costs
about a second of local GPU per item and acts on the consequence. `T21-11` did the asking first,
deliberately, and the translator survives as a **safety net** for what instructions cannot reach:
verbatim quotes the author pastes, other MCP clients, and small local models that follow
instructions unreliably. How much net is needed is measured, not assumed — `stats`'
`memory.normalization` block reports the ratio of detected-English to translated.

## Consequences

- **The code base shrinks.** `crates/store/src/memory/effective_text.rs`,
  `crates/local-rag/src/daemon/normalization/write.rs`, the triplicated subject hashing and its
  source lint all exist solely because an entry had two texts. One canon removes them — roughly a
  thousand production lines net negative — and removes with them a defect nobody had registered:
  every normalization permanently orphaned the entry's previous vector in `embedding_cache`, which
  no sweep reclaims (`crates/embed/src/backfill.rs` visits only *expected* keys).
- **`remember` and `recall` can pay for inference.** Only non-English input does: the script
  detector is pure, model-free and free (ADR-0010 Decision 8, still in force), so an English
  request costs nothing measurable. A non-English one costs roughly a second of local generation.
  This is the cost ADR-0010 Decision 3 refused, accepted here with the mitigation that `T21-11`
  makes the non-English path rare.
- **Memory quality now depends on the local translator.** The live run behind `T21-08` failed on
  3 of 18 real entries — a single-line JSON envelope tearing on long text. Tolerable when a failed
  translation merely left an entry unnormalized; not tolerable once the canon depends on it, which
  is why `T21-16` is a blocking card and not a cleanup.
- **The retrieval benefit remains unproven.** See §Context. `T21-18` builds the corpus that can
  test it; until then this decision stands on coherence and the owner's product call, and any
  later claim of an MRR gain must cite that run, not this ADR.

## Alternatives rejected

- **Keep the original as canon and feed the English variant to both legs.** Retrieval-equivalent,
  and it preserves spec 08 §3 untouched — but it keeps every structure §Consequences lists as
  removable, permanently: two texts, a computed-hash join, an effective-text decision function and
  the orphaned-vector defect. The owner chose the simpler store.
- **Change the fusion instead.** `D-075` showed RRF buries a single-leg dense hit under any
  document both legs found, which is a real and separate finding. It would have addressed the
  symptom on this corpus without giving the store a coherent language, and it edits a
  `[FIXED pipeline]` shared with code search for a memory-specific reason. Left as its own
  question, not folded in here.
- **Translate the query only, leaving the store mixed.** `X-011`'s `query_en` moved the overall
  number but regressed `ru-ru` (1.0000 → 0.9375) — an English query genuinely matched against a
  Russian store is worse, not better. Query normalization is adopted here *together with* an
  English store (§Decision 2), which is the configuration that measured 1.0000.
