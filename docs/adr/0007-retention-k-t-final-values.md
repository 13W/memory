# ADR-0007: Retention `K`/`T` final values (v0/GA)

## Status

Accepted — 2026-08-05.

Closes open question **O6 "Retention `K`/`T` for retired generations"**
([spec 15 §4](../specification/15-roadmap.md)), left explicitly open at `G17`
("boundary made explicit, not resolved", [spec 06 §5](../specification/06-reconcile-and-fts.md)
as-built note, T17-05). Delivered by task **X-001**, minted post-`G17` under the
`X-NNN` scheme `TRACEABILITY.md` reserves for work "created only on an explicit
product decision" once the `T00–T17` queue is closed. Follows the ADR
convention established by ADR-0001/0002/0003 for closing a spec `[OPEN]` marker
by owner decision rather than by spike/benchmark.

## Context

Spec 06 §5 `[FIXED]`-mechanism pin-roots block names the retention window as
`last K retired generations OR retention window T (rollback/debug)
[OPEN: K, T]`. T06-01/T06-02 built the mark/sweep mechanism as fully
config-driven — `K`/`T` are read from `[storage]` (`retired_generations_keep`,
`retired_generations_ttl_h`), never hard-coded constants — with provisional
defaults `K = 2`, `T = 168h`. The mechanism itself has never been in question;
only whether these two numbers are the final, normative answer.

T17-05's own as-built note (spec 06 §5) is unambiguous about why the numbers
were never re-derived: "no usage-metrics telemetry exists anywhere in this
codebase — no counter or log records how often a `retiring` generation is
actually consulted after retirement — and this task adds none: building that
telemetry is a separate project (its own schema, its own privacy/`local_only`
review under the data-policy guard, 12 §1-2) rather than something a
release-report task can produce as a side effect." `G17` accepted this as a
legitimate v0 boundary and named the resolution path explicitly: "Whether GA
re-derives `K`/`T` from real telemetry (versus formally keeping the
provisional defaults permanent) remains the actual open product decision,
tracked as a pre-GA release-gate item alongside O2/O5." `15-roadmap.md`'s O6
row and `TRACEABILITY.md`'s O6 row both restate the same two-path framing.

**A visible tension worth naming explicitly.** `docs/implementation-plan/groups/06-retention-and-gc.md`'s
`G06` gate text says "O6 не считать закрытым без данных" ("O6 must not be
considered closed without data") — written early, at group 06, before the
project's later framing existed. By `CLAUDE.md`'s own precedence order,
`docs/specification/*` (item 2) outranks `docs/implementation-plan/groups/*`
(item 5): the later, higher-precedence spec text (`15-roadmap.md`'s O6 row,
amended at T17-05/`G17`) explicitly lists "formally keeping the provisional
defaults permanent" as a legitimate, data-free closure path, on equal footing
with telemetry-driven re-derivation. This ADR resolves O6 through that
higher-precedence path — a deliberate owner product decision, not a silent
override of `G06`'s earlier, narrower phrasing, and not a case requiring the
`DEVIATIONS.md` workflow: `DEVIATIONS.md`'s own closing rule states "Для
`[OPEN]` решения здесь ссылаются на ADR/spike, но не превращают вопрос в
deviation, если реализация ещё не нарушена" — the retention mechanism was
never in violation, so no `D-NNN` entry is filed.

No spike, benchmark, or telemetry project was commissioned for this decision.
The owner reviewed the above context in this session and chose the
"keep provisional permanent" path over commissioning a telemetry project.

## Decision

**`retired_generations_keep = 2` (`K`) and `retired_generations_ttl_h = 168`
(`T`) are the final, normative v0/GA values.** They are no longer provisional
placeholders awaiting evidence — they are the deliberate, permanent defaults,
chosen without usage telemetry because building that telemetry was judged not
worth commissioning as a prerequisite to GA.

Nothing about the mechanism changes: `K`/`T` remain ordinary `[storage]`
config fields (`crates/core/src/config::StorageConfig`), never constants — an
operator MAY still override them per-deployment via `config.toml`. Only their
epistemic status changes, from "placeholder, pending data" to "decided
default".

## Consequences

- Spec 06 §5's pin-roots block loses the `[OPEN: K, T]` marker in favor of a
  citation to this ADR; a new as-built note is appended (not replacing
  T06-01/T06-02/T17-05's notes, which remain accurate historical record of
  what was true at each of those tasks) recording the resolution.
- Spec 15 §4's O6 row is marked `**RESOLVED — ADR-0007**`, matching the style
  already used for O1/O4/O7's rows.
- `TRACEABILITY.md`'s O6 row moves from "легитимно открыт для v0" to
  "resolved", citing this ADR and task X-001.
- `crates/store/src/retention.rs`'s three doc comments that call `K`/`T`
  `[OPEN]`/"provisional, not normative" (module doc, `RetentionParams` struct
  doc, `from_storage_config` method doc) are updated to cite this ADR instead.
- `crates/core/src/config/mod.rs`'s `StorageConfig` struct doc and the two
  affected field docs drop "`[OPEN]` in the spec... not a closed answer" in
  favor of a citation to this ADR.
- No behavior, default value, migration, or test changes: `K = 2`/`T = 168h`
  were already the shipped defaults; this ADR only changes their documented
  status.
- A future re-derivation from real usage telemetry remains possible as a
  separate, later decision — this ADR does not preclude it, and does not
  itself commission any telemetry work. Revisiting these numbers later would
  need new evidence (a telemetry project's data), not a reinterpretation of
  this ADR's reasoning, mirroring the pattern ADR-0003 (§ Consequences) set
  for revisiting the dense-backend choice only on new cardinality/latency
  evidence.
