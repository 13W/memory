# local-rag v2 — behavioral fixtures (imported from v1)

Implementation-neutral fixtures produced by task **T00-01**. They capture the *observable
behavior* inherited from v1 (`input tree / event stream / query → expected behavior`) **before**
any rewrite code, so the Rust rewrite can be checked against v1's behavior rather than against
v1's internals. No vector-store payload schema is imported (01 §7, 14 §1).

Source: v1 repository `local-rag` (TypeScript), commit `31dfba2`. Provenance for every case
points back to the exact v1 test/script it was distilled from.

## Layout

```
fixtures/
  manifest.json        # source provenance, per-family coverage, gap register, baseline inventory
  README.md            # this file
  schema/              # JSON Schema (Draft 2020-12) — the normative shape of each data file
  tooling/validate.py  # bootstrap validator (stdlib; uses jsonschema if installed)
  search/corpus.json   # the 49-query benchmark corpus (query -> single ground-truth target)
  search/baseline/     # captured v1 baseline run (metrics + provenance); thresholds stay TBD
  reconcile/index.json # skip-policy fixtures (gitignore stacking, git worktree)
  memory/index.json    # store/dedup/ttl/status, scoring, transcript pipeline, router extraction
  adversarial/index.json # untrusted LLM-output robustness (reasoning/CJK leak, malformed JSON)
  fault/index.json     # LLM retry/backoff, rate-limit queue, rejection safety
  fault/matrix.json    # declarative F1-F12 (05 §10) + S1-S8 (07 §7) matrices
  parser/README.md     # gap-only: no v1 chunking goldens (see GAP-01)
```

## Conventions

- **Format**: JSON, validated by JSON Schema in `schema/`. Each family index is
  `{family, version, cases[]}`; each case is `{id, title, status, provenance, input, expected}`.
- **id**: globally unique across all families and the search corpus (checked by the validator).
- **status**: `active` = v0 expected behavior; `deferred` = kept for reference, not a v0 gate.
- **input/expected**: observable behavior only — never internal storage/payload field names.
- **No backend fields**: keys in the denylist (`manifest.json → denylist`, e.g. `file_path`,
  `code_vector`, `payload`, `collection`, `parent_id`, …) MUST NOT appear anywhere. The
  validator enforces this — it is the machine-checkable proof that no vector-store schema leaked
  in.
- **Thresholds are TBD**: gate numbers (MRR/Recall@5, router P/R, latency/resource p95) are
  `[BASELINE]`/`[OPEN]`. We record the *metrics* and the v1 baseline *numbers*, never invented
  *thresholds* (O2: "collect metrics, do not invent thresholds"). See `manifest.json → baseline`.

## Validate

```
python3 fixtures/tooling/validate.py        # stdlib only; exit 0 = green
# optional, for authoritative Draft 2020-12 validation:
python3 -m pip install -r fixtures/tooling/requirements.txt
```

The validator runs the four T00-01 checks: schema validation, id uniqueness, runner dry-run
(loads every fixture + referenced input artifact), and the no-backend-field denylist scan.

## Coverage & gaps

Six fixture families (14 §1). Each has either imported fixtures or an explicitly registered
blocking gap in `manifest.json → gaps` (parser is gap-only: v1 has no chunking goldens).
Gaps resolve in later tasks (e.g. parser goldens in T04, generation diffs in T05, F/S executable
scripts in T00-03/T07-05/T13-06, rev6 memory-op corpus in T14-07). Deferred v0 scope (15 §3) is
listed under `manifest.json → deferred` and is never encoded as v0 expected behavior.

This is a bootstrap: T00-03's Rust fixture/failpoint harness will consume these same files and
validate the same schemas.
