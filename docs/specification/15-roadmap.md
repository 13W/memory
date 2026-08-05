# 15 — Roadmap, MVP, Open Questions

## 1. Implementation order `[FIXED]`

**Can start now (core storage, before any dense backend) — in parallel with design rev cycles:**

1. Migration framework (13 §3).
2. Repository/worktree registry — **stable worktree UUID + `worktree_path`** — and path
   normalization (03 §2.1, §1.3).
3. Exact file revisions / `source_blob` / `parser_fingerprint` / `skipped_file`.
4. `parsed_unit` + parser fixtures + uniqueness constraints.
5. Generation membership + structural-sharing tests + deterministic occurrence IDs.
6. Strict reconcile without embeddings + retention/GC pin roots.
7. **Fake projection backend** + fault injection: detection matrix + rebuild tests.

**Before any real dense backend:**

8. Write-ahead + validate-on-open protocol (05) on the fake backend, incl. two-axis
   `worktree_projection_state`.
9. `fts_projection_head` + degraded-mode semantics (06 §4).
10. Shard strategy (per-worktree) + hybrid read locking (02 §5).

**Then:**

11. **Comparative dense-backend spike**: Qdrant Edge vs usearch vs brute-force over
    `embedding_cache` — metrics per 14 §7. **Backend fixed here, not earlier** `[FIXED]`.
12. Embedding cache / representations registry / model spaces + per-worktree activation.
13. FTS5 + dense RRF + benchmark baseline.
14. Spool-only ingestion + observations + cursor/lease + **memory-quality fixture set**.
15. Memory state machine + evidence + review tools + **router gate**.
16. Description leg / reranker — only after baseline.
17. Daemon lifecycle (+ store/config discovery) + platform packaging.

Steps 1–7 depend on **no** open question `[FIXED]`.

## 2. MVP (v0) scope `[FIXED]`

Identities/protocols fixed, logic minimal: Rust binary + minimal CC plugin/launcher;
`state.sqlite` + `cache.sqlite` + migration framework; repo registry + stable worktree UUID +
generation + locking; tree-sitter for TypeScript, JavaScript, Rust (ADR-0001, closes O4);
`file_revision`
(+source_blob+parser_fingerprint) + `parsed_unit` + occurrences + structural sharing +
`skipped_file`; authoritative reconcile + write-ahead switch + validate-on-open (per-worktree
shard); one embedding model + representations registry; `embedding_cache`; **dense leg = the
simplest backend that passes the benchmark** (possibly brute-force — step 11 decides);
FTS5(occurrences) + `fts_projection_head` + RRF under read lock; 49-query benchmark baseline;
spool-only ingestion + envelope/payload + idempotent cursor/lease consolidation;
memory-quality fixture set + router gate; recall v0 (FTS + brute-force cosine); memory state
machine + evidence + `list/approve/reject/edit`; fault injection: detection matrix +
rebuild correctness.

## 3. Deferred (all additive) `[FIXED]`

LLM descriptions; reranker; fine-grained evidence scoring; full recall; ANN for memory;
multiple generators; cross-generation matching; LSP graph; multi-harness; FreeBSD;
`win32-arm64`; v1 `find_usages`/`get_dependencies` parity tools until graph semantics fixed.

## 4. Open questions register `[OPEN]`

| # | Question | Resolved by | Blocks |
| --- | --- | --- | --- |
| O1 | Dense backend (Qdrant Edge / usearch / brute-force) | **RESOLVED — ADR-0003 (T10-05): brute-force** | steps 12+ dense specifics only |
| O2 | Gate numbers (quality/latency/resources; memory router P/R) | **search quality RESOLVED — T12-05 thresholds (`X = 0.03` MRR budget / `Y = 0.80` Recall@5, versioned in `fixtures/search/baseline/thresholds.json`, derived from the agreed v1 baseline) now PASS on the shipped default: MRR 0.7007 against the 0.6963 baseline, Recall@5 0.8367 — reached via D-016/D-017/D-018's resolution chain (corpus/window scope, the provider reading the model's pooled output, weighted RRF), all `resolved`**; **memory-router P/R RESOLVED — T14-07/ADR-0006/T14-09: `P = 0.60`, `R = 0.50`, versioned in `fixtures/memory/baseline/thresholds.json`, derived from the real `gemma-4-e2b-it-gguf-q4-0` native-template baseline run**; **latency/resources RESOLVED as this release's first-established v2 baseline — T17-05: `cargo xtask release-report` measures warm-search + one-file/branch-checkout reconcile p50/p95, idle RAM, bytes/occurrence, embedding-cache-budget adherence, and source/worktree byte ratio end to end, deliberately never gated (no prior measurement to regress against); real numbers and raw artifacts in `fixtures/release/run-2026-08-05.json`/`.report.md`, cited in 14 §2's own as-built note** | release criteria only |
| O3 | Default embedding model + weights delivery; local generator crate | **embedding model RESOLVED — ADR-0004 (T11-03): `embeddinggemma-300m`, 768d, cosine**; **weights delivery RESOLVED — ADR-0005 (T11-06): `ort` + `load-dynamic`, q8, pinned-digest atomic installer** (closes `D-008`); **local generator crate RESOLVED — ADR-0006 (T14-07): `llama-cpp-2`, `Gemma 4 E2B` q4_0, greedy decoding (revised from an initial `Qwen2.5-0.5B-Instruct` pick once a real Gemma 4 comparison run — requested by the user — scored roughly double the F1); generalized chat-template support without per-model hardcoding RESOLVED — T14-09: `minijinja` renders each model's own raw embedded template directly, no per-entry `chat_template_override` needed** | init UX, router quality |
| O4 | First-release language set | **RESOLVED — ADR-0001 (T04-01): TypeScript, JavaScript, Rust** | parser scope |
| O5 | v1 memory migration vs clean start | product decision before GA — boundary made explicit at T17-04 (13 §3's own as-built note): v0 ships clean-start only; the open half is whether GA adds a real importer, tracked as a GA release-gate item, not silently dropped | 13 §3 last step |
| O6 | Retention K / T for retired generations | **RESOLVED — ADR-0007 (X-001): `K = 2`, `T = 168h` adopted as final v0/GA values** — owner product decision, not telemetry-derived; boundary made explicit at T17-05 (06 §5's own as-built note) that no usage-metrics telemetry exists anywhere in this codebase, and building it was judged not worth commissioning as a GA prerequisite | GC defaults |
| O7 | Final `SyntaxLocator` / graph semantics | **`SyntaxLocator` derivation RESOLVED — ADR-0002 (T04-03); graph semantics = design follow-up** | `find_usages`/`get_dependencies` (graph only) |
| O8 | One shared DB vs `state`+`cache` at scale | revisit on growth; **split now** `[FIXED]` | nothing |

## 5. Spec growth plan

Per rev 6 §18, this spec is grown per implementation step, not frozen: each step lands with
its section updated to as-built precision (DDL diffs via migrations, protocol clarifications
as `[SPEC]` amendments). `[SPEC]` items are the review queue for step kick-offs; `[FIXED]`
changes require a design rev 7.
