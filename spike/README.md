# Dense-backend spike (`spike/`)

An **isolated workspace** for the roadmap step-11 comparative dense-backend spike
(open question **O1**: Qdrant Edge vs `usearch` vs brute-force over the embedding
cache — spec 05 §1, 14 §7, 15 §4). It closes O1 by **measurement**, not by an
implementer's preference; the backend is fixed here, never earlier.

## Why a separate workspace

This directory is a standalone Cargo workspace with its **own `Cargo.lock`**, and
is `exclude`d from the repository-root workspace. The candidate adapters this spike
will grow (brute-force — T10-02; `usearch` — T10-03; Qdrant Edge — T10-04) pull real
dense-vector crates. Keeping them here means the **product** `Cargo.lock` never
contains a dense-backend SDK, so the "no premature backend coupling" guardrail
(`CONTRIBUTING.md` § Dependency policy) stays *mechanically* checkable through G17.
The winning backend is copied into the product workspace at T12-02.

## What T10-01 shipped

Only the comparison infrastructure — not any candidate, not the choice, not any
production search integration:

- **`corpus`** — seeded, byte-reproducible synthetic datasets (`small`/
  `representative`/`large`). Vectors are synthetic (no embedding model); point IDs
  come from the real `projection_point_id`, so a dataset yields a valid manifest.
  `small = 544` points / `dim = 768` are the *measured* v1 baseline
  (`fixtures/search/baseline/`), not invented numbers; the larger sizes are
  `[SPEC]`-provisional (revisited at T10-05).
- **`report`** — the fixed 14 §7 metric matrix as typed, `deny_unknown_fields`
  serde structs (deserialization *is* the schema check).
- **`conformance`** — the shared `ProjectionStore` contract suite every candidate
  runs through: reopen / head / manifest / on-disk corruption detection.
- **`SpikeAdapter`** + **`FakeAdapter`** — the candidate abstraction, with the
  product fake backend as the reference candidate. An unsupported target is
  *reported* (`supported: false` + reason), never silently skipped.
- **`spike` binary** — runs one adapter × one dataset, writes a JSON report.

## Run

```sh
# Tests (also run by `cargo xtask ci` from the repo root):
cargo test --manifest-path spike/Cargo.toml

# Generate a benchmark report artifact:
cargo run --manifest-path spike/Cargo.toml --bin spike -- \
    --adapter fake --dataset small --seed 42 --out spike/artifacts/fake-small.json
```

`spike/artifacts/` holds committed, reproducible spike outputs. Timing/RAM numbers
in a report are **measurements**, not thresholds (O2: collect metrics, never invent
thresholds); only report *shape* and *conformance* are asserted by tests.
