# 07 — Observations: Spool-Only Ingestion

**`[FIXED]` Hooks never talk to the daemon for ingestion.** The only ingestion path is a
durable spool append. This removes the ACK protocol entirely (no "daemon accepted bytes but
crashed before commit" state — the durable moment of an event is *defined* as a successful
atomic append) and removes dual-path identity (rev 5 required spool and direct IPC to compute
bit-identical source identity; two code paths with that contract always diverge).

Cost: seconds of delay before consolidation — irrelevant for memory. Fail-open is trivial:
daemon unavailability does not affect the hook path at all.

## 1. Capture set `[FIXED]`

`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure`, `Stop`,
`SubagentStop`, `SessionEnd`. Delivery is **at-least-once**; dedup happens at import (§5).

`stop_hook_active` is NOT a headless indicator; hooks record observed properties only `[FIXED]`.

## 2. Hook write path `[FIXED steps, format [SPEC]]`

```
parse hook JSON (stdin)
→ REDACTION (before anything touches disk; 12 §2)
→ compute source identity (computed exactly once, at write time [FIXED])
→ build frame → flock(segment) → single write(O_APPEND) → fdatasync → funlock
→ exit 0 (always; any internal error ⇒ silent fail-open)
```

- Segments are **per-session**: `spool/<session_id>/<seq:06>.seg`. Per-session segments plus an
  exclusive `flock` during append `[SPEC]` eliminate interleaving (O_APPEND alone does not
  guarantee non-interleaving of large writes across processes `[FIXED rationale]`; concurrent
  PostToolUse hooks within one session are possible). Windows: `LockFileEx` on the segment.
- Rotation: writer opens a new segment when the current one exceeds 8 MiB `[SPEC]`; `seq`
  strictly increasing; a writer never appends to a segment below the current max seq.
- Permissions: dirs 0700, segments 0600 / restrictive ACL `[FIXED]`.
- Size caps `[FIXED caps exist, numbers [SPEC]]`: payload cap 256 KiB per event (truncated with
  hash + metadata per 12 §2), frame cap 1 MiB (larger frames are invalid by format).

As-built note (T13-01, `[SPEC]`): the REDACTION step is
`local_rag_hook::payload::prepare_payload` (`crates/local-rag-hook/src/payload.rs`), a pure,
event-shape-agnostic transform over an already-extracted payload string/paths/tool-name (parsing
the actual hook JSON into those fields is T13-02's). Order is **redact, then cap**: a secret
sitting near the 256 KiB boundary must not survive by escaping redaction inside a half-truncated
value. Deny-list exclusion (12 §2, `local_rag_core::config::SpoolConfig`) is checked **first** —
a denied event's raw payload is never even scanned, so envelope-only really means no payload
content ever reaches this module's output in any form, not merely a redacted-away one. The
underlying scanner verdict is `local_rag_core::redaction::Scanner` (T03-02); this task adds the
missing masking half, `Scanner::redact` (spec 12 §2's as-built note has the detail), reused
verbatim rather than reshaped, so the flow this section names ("spool ingestion… reused") and
group 16's future remote-transmission flow share byte-identical redaction behavior. `compute
source identity` and `build frame` remain T13-02's, unstarted here.

As-built note (T13-02, `[SPEC]`): the remaining pipeline steps are real. `local_rag_hook::event`
parses the real Claude Code hook JSON (an external contract this project does not control) into a
typed `ParsedEvent`/`EventPayload` per capture-set member — only the fields spec 07 §4's identity
table actually needs (`session_id`; `tool_use_id` for PostToolUse/Failure; `agent_id` for
SubagentStop; `prompt` for UserPromptSubmit) are hard requirements, everything else is optional,
and an unrecognized `hook_event_name` fails open rather than crashing (forward-compat with a
future Claude Code hook type). `local_rag_hook::identity::compute_identity` implements this
section's table (as-built detail at §4 below). `local_rag_hook::frame::{encode_segment_header,
encode_frame}` build the wire bytes (§3's as-built note has the `payload`-field detail);
`local_rag_hook::segment::append_frame` does the lock-then-rotate-then-append: the rotate-or-not
decision is made from `file.metadata()` read only **after** `std::fs::File::lock()` succeeds
(the same portable, dependency-free `flock`/`LockFileEx` idiom `crates/store/src/migrate/
lock.rs`'s `MigrationLock` already established for the L1 lock), never from an earlier unlocked
scan — closing both the "two writers both rotate" and "a write pushes a segment over threshold
while another is mid-decision" races. `crates/local-rag-hook/src/main.rs`'s `spool-write`
subcommand wires the whole path end to end: every fallible step is a typed `Result`
(`HookError`), plus `std::panic::catch_unwind` as a safety net (the workspace sets no
`panic = "abort"` profile override, so unwinding is real here), converging on `ExitCode::SUCCESS`
unconditionally. The 200 ms budget (11 §3.1) is measured, not enforced as a hard deadline — killing
mid-write would risk an inconsistent lock/file state — and reported via `eprintln!` only past the
fact (no logging subsystem exists anywhere in this workspace yet). `worktree_root` is the hook
JSON's raw `cwd` field, uncanonicalized, and `commit` stays `null`: git introspection is a
daemon-side concern (02 §3.3/03 §2.1's as-built notes: `local-rag-store` carries no git
dependency), and the hook must stay exec-fast (13 §1 "<50 ms cold") — canonicalizing here would
add real syscalls for no benefit, since the daemon re-resolves identity from the raw root at
import regardless. `paths` is extracted from `tool_input` by scanning well-known key names
(`file_path`/`path`/`notebook_path`) rather than a per-tool switch — deliberately over-inclusive
(a false positive is harmless to the deny-list gate; a false negative is the security-relevant
failure mode), and deliberately not attempting to parse paths out of Bash's free-text `command`
(`SpoolConfig::deny_tools`, T13-01, is the right lever for that). `evidence_kind`/`trust` per
event type (not provided by Claude Code's own JSON) are this project's own classification:
`PostToolUse`/`Failure` → `tool_result`/`normal`; `UserPromptSubmit` → `user_statement`/`high`
(the user is the most authoritative, unmediated source available); `Stop`/`SubagentStop` →
`model_claim`/`low` (directly justified by 12 §4 `[FIXED]` "model-claims are never auto-promoted
to facts" — both carry the model's own generated `last_assistant_message`); `SessionStart`/
`SessionEnd` → `code_state`/`normal` (by elimination — no tool ran, no party "stated" or
"claimed" anything). `short_evidence_excerpt` is left `null` at write time — it is not this
task's to populate (see 12 §2's as-built note on the 4 KiB evidence-excerpt cap remaining group
14's).

As-built note (T13-03, `[SPEC]`, corrects and extends T13-02's note above): the frame reader is
now real. The wire-format primitives named just above —
`local_rag_hook::frame::{encode_segment_header, encode_frame}` — **relocated**: they now live in
`local_rag_core::spool` (module `crates/core/src/spool.rs`), moved verbatim out of
`local-rag-hook`'s now-deleted `frame` module, plus a new symmetric decoder-side primitive,
`local_rag_core::spool::decode_segment_header`/`SegmentHeader`/`HeaderError`. `local-rag-hook` is
a leaf binary crate (13 §1) with nothing else depending on it as a library, while the new
daemon-side decoder is daemon-side; relocating to `crates/core` (already the shared foundational
crate for identity/hash/config/paths/redaction) lets the writer and the reader depend on exactly
one CRC/layout implementation rather than risking two that could drift — the same "single shared
component" posture as this project's `Scanner`/`tokenize_identifier`. The decoder itself
(`local_rag_store::spool`, `crates/store/src/spool.rs`) is a pure `&[u8] → DecodedObservation`
transform with no database awareness — the `observation_envelope`/`spool_import_cursor` DDL (spec
03 §2.5) and the actual transactional import remain T13-04's, which consumes this module's
decoded, classified output. `decode_segment` validates the 16-byte header first and returns `Err`
immediately on an unsupported (newer) format version, attempting zero frames; `decode_frames`
then decodes as many whole frames as possible from the remainder, stopping cleanly at a torn tail
(§3's as-built note below has the corruption/torn-tail distinction).

## 3. Segment wire format `[SPEC]`

```
Segment header (16 bytes):
  magic   "LRSP"            (4)
  version u16 LE = 1        (2)
  flags   u16 LE = 0        (2)
  reserved                  (8)

Frame (repeated):
  len      u32 LE           payload byte length (≤ 1 MiB)
  crc32c   u32 LE           over payload bytes
  payload  len bytes        canonical JSON, UTF-8
```

Frame payload fields:

```json
{
  "format_version": 1,
  "source_event_id": "…",          // §4 — identity, computed at write
  "dedup_key": "…" | null,         // only for stable-identity events
  "event_type": "PostToolUse",
  "captured_at": 1789...,
  "session_id": "…", "agent_id": null, "turn_id": null, "batch_id": null,
  "worktree_root": "/canonical/path" | null,
  "commit": "…" | null,
  "evidence_kind": "tool_result",
  "trust": "normal",
  "paths": ["src/a.ts"],
  "payload": { /* redacted event body */ },
  "short_evidence_excerpt": "…"
}
```

**Durable moment of an event = successful atomic append (write + fdatasync)** `[FIXED]`.
A torn tail frame (bad len/CRC at EOF) is by definition a *non-durable* event: importer stops
at it; the appending hook holds the flock until its frame is complete, so no valid frame can
follow a torn one within a segment `[SPEC]`.

As-built note (T13-02, `[SPEC]`, amends the illustration above): **`payload` is encoded as a
JSON string, not a raw nested object.** `local_rag_hook::payload::PreparedPayload` (T13-01) only
guarantees its redacted/capped bytes are valid UTF-8 — not valid JSON, since a truncation can land
mid-structure. Embedding that content as a literal nested object would make the *whole frame*
invalid JSON whenever a payload happened to be capped. Encoding `payload` as an ordinary JSON
string (double-encoded — a string whose unescaped content is itself JSON text in the common,
uncapped case) sidesteps this: the outer frame is structurally always valid JSON by construction
of `serde_json`'s derived `Serialize` over a typed `FramePayload` struct, regardless of what
happened to the inner content. `payload` is `null` for an envelope-only (denied) event. Field
order in `FramePayload`'s declaration matches this section's illustration exactly, so
`serde_json`'s field-declaration-order output is what "golden wire bytes" tests pin.

As-built note (T13-03, `[SPEC]`): the decode side is real and symmetric to the encoder. A
frame's `len` is checked against `MAX_FRAME_PAYLOAD_BYTES` **before** checking whether enough
trailing bytes exist: an impossible length can never come from a legitimate in-progress write, so
it is corruption regardless of what follows, whereas a *legal* `len` with insufficient trailing
bytes is a torn tail — not an error, since "the appending hook holds the flock until its frame is
complete, so no valid frame can follow a torn one within a segment" (above) means the importer
simply stops and resumes later. A buffer that ends exactly on a frame boundary is a distinct,
clean outcome (no trailing bytes at all), never confused with a torn tail. `FramePayload` gained
`Deserialize`/`PartialEq`/`Eq` (additive to its existing derives) so the decoder can deserialize a
frame and tests can compare decoded payloads structurally.

## 4. Source identity per event type `[FIXED]`

| Event | `source_event_id` | `dedup_key` (stable → UNIQUE) |
| --- | --- | --- |
| PostToolUse | `pt:<session>:<tool_use_id>:ok` | same (stable) |
| PostToolUseFailure | `pt:<session>:<tool_use_id>:fail` | same (stable) |
| SubagentStop | `ss:<session>:<agent_id>:<stop_occurrence>` | same (stable) |
| UserPromptSubmit | best-effort fingerprint `up:<session>:<H(prompt)>:<coarse_ts>` | **null** |
| Stop | `st:<session>:<H(context)>:<coarse_ts>` | **null** |
| SessionStart / SessionEnd | `se:<session>:start/end:<coarse_ts>` | **null** |

Two legitimate identical prompts / Stop events are legal — best-effort fingerprints are
**never** under a UNIQUE constraint `[FIXED]`; their dedup is a bounded retry window at import
(§5). Guarantee `[FIXED]`: events with stable source IDs deduplicate exactly; the rest
best-effort; consolidation and memory ops are idempotent regardless.

As-built note (T13-02, `[SPEC]`): three numbers/decisions this table left open are now fixed by
`local_rag_hook::identity`.

- **`coarse_ts` = 1-second buckets** (`captured_at_ms / 1000`). Coarse enough to absorb a
  duplicate hook invocation for the same real event landing within the same second, without
  widening the false-collision window meaningfully beyond what the *separate*, later import-side
  bounded dedup window (§5, 10 min / 512 envelopes) already tolerates by design. No principled
  derivation fixes this number exactly; it is this task's concrete pick.
- **`stop_occurrence`**: Claude Code's `SubagentStop` event carries no occurrence counter, and the
  hook is a fresh, stateless process per invocation, so a durable, monotonic count has to live on
  disk — `local_rag_hook::subagent_counter`, a small per-session, per-agent JSON counter file
  (`spool/<session_id>/.subagent_stop_seq.json`) updated under its **own**, separate, never-renamed
  lock file (`.subagent_stop_seq.lock`) via the same `File::lock()` idiom as the segment writer; the
  counter value itself is replaced write-new + `fdatasync` + atomic `rename` (never truncate-in-
  place, which is not crash-atomic). A corrupt counter file is a hard error — skip *this*
  `SubagentStop` event (fail-open) — never a silent reset to `{}`, since a reset risks reissuing an
  occurrence already used by a previously-imported envelope for the same agent (a false
  `dedup_key` collision against permanently stored history is worse than losing one observation).
  What this mechanism does and does not guarantee: Claude Code never learns a hook invocation
  failed (`[FIXED]` fail-open, always exit 0), so it almost certainly never *deliberately* retries a
  hook call — the "at-least-once delivery" language above describes the general spool-crash story,
  not a literal retry loop for this specific event. The counter correctly guarantees distinct real
  stops always receive distinct, monotonically increasing numbers, and that a crash mid-append
  (S1) never corrupts or skips the count (the counter update and the segment append are two
  independent durable operations). It structurally **cannot** distinguish "Claude Code double-fired
  the hook for one logical stop" from "two genuinely distinct stops" — Claude Code provides no
  correlating signal for that, so every invocation gets a fresh number by design. An
  information-theoretic limit, not something better engineering closes.
- **`H(prompt)`/`H(context)` = plain `local_rag_core::hash::sha256_hex`, not a new
  `local_rag_core::identity::domain::Domain` variant.** The domain-separated BLAKE3 family is
  reserved for values backing a durable, retry-stable, schema-level identity — an FK target or a
  UNIQUE lookup key (every existing `Domain` variant does exactly that). These fingerprints are one
  segment of a compound string that is **explicitly never** under a UNIQUE constraint (this
  section, above) and never itself a stored identity column — the same shape as
  `subject_memory_entry`'s own inner `H(text)` (03 §1.2's as-built note), which is documented as
  deliberately using plain `sha256_hex` for the identical reason. `Stop`'s "context" resolves to
  `last_assistant_message` (defaulting to an empty string if absent) — the only Stop-specific field
  Claude Code exposes; this section never defined "context" more precisely, so this is this task's
  interpretation, not a re-derivation of something already fixed.

As-built note (T13-03, `[SPEC]`): the read side of this table is implemented by
`local_rag_store::spool`'s private `classify` function, which checks `event_type` against this
table's stable/best-effort split and cross-checks it against the frame's actual
`dedup_key.is_some()`. A mismatch (e.g. a `PostToolUse` frame with a `null` `dedup_key`, or a
`Stop` frame with one present) is reported as `ClassificationError::DedupKeyEventTypeMismatch` and
stops decoding at that frame as corruption — an explicit defense against an internally
inconsistent frame (corrupted, or from a future bug) poisoning T13-04's `UNIQUE(dedup_key)`
import logic. The result, `DedupClass::Stable{dedup_key}` / `DedupClass::BestEffort`, is a named,
tested classification rather than an ad-hoc `.is_some()` check scattered at import call sites.

## 5. Import (daemon side) `[FIXED protocol, mechanics [SPEC]]`

```
notify/debounce tail of spool dirs
→ for each session, from spool_import_cursor(segment_seq, committed_offset):
    read frames; verify len/CRC; torn tail ⇒ stop (retry later)
    for each frame:
      stable identity  → INSERT envelope; UNIQUE(dedup_key) conflict ⇒ skip (exact dedup)
      best-effort      → skip if an envelope with same source_event_id exists within the
                         bounded window (same session, received within [SPEC] 10 min /
                         last [SPEC] 512 envelopes)
      assign received_seq (AUTOINCREMENT) — transactional, monotone [FIXED]
    ONE tx per batch: envelopes + observation_paths + payloads (TTL set)
                      + advance spool_import_cursor
→ truncate/delete a segment ONLY after its bytes are ≤ committed cursor [FIXED]
```

Ordering: `received_seq` is the cursor basis; **order ≠ causality** — causality is carried by
`tool_use_id` / `turn_id` / parent / `batch_id` `[FIXED]`.

Envelope resolution at import: `worktree_root` → `worktree_id`/`repo_id` via registry; an
unknown root imports with NULL worktree (repo/global scoping still possible later).

As-built note (T13-04, `[SPEC]`): the transactional batch importer is
`local_rag_store::observation::import_batch`, which composes exactly this pseudocode's inner loop
into one `StateWriter::transaction`: resolve `worktree_root` **once per batch** (not per frame —
in practice every frame decoded together in one pass shares one session, and a session's `cwd`
does not change mid-batch) via `registry::resolve`, insert envelopes/paths/payloads, and advance
`spool_import_cursor`, all committing atomically. `registry::resolve` needs a canonicalized,
git-probed `RequestRoot` (`crates/store/src/registry/resolve.rs`'s own doc: building one from a
raw path is "the daemon's job (T15)" — `crates/store` carries no git dependency). Since group 15
has not started, `import_batch`/`import_session_tail` accept an already-built `&RequestRoot` as a
parameter rather than computing one from the frame's raw `worktree_root` string themselves;
passing `RequestRoot { worktree_root: None, .. }` (today's only available caller state) resolves
to `GlobalOnly`, which **is** this section's "an unknown root imports with NULL worktree" —
literally, not a stand-in for it. A future group-15 driver supplies real git-probed facts through
the same parameter without this module changing. `Resolution::Ambiguous` is treated the same as
`GlobalOnly` (NULL ids): an ambiguous root has not picked one specific worktree, so recording no
worktree is the conservative reading (never guessing).

`local_rag_store::observation::import_session_tail` is the per-session driver: it reads the
current `spool_import_cursor` (absent ⇒ start of segment 1), decodes via T13-03's
`local_rag_store::spool::{decode_segment, decode_frames}` — walking across a segment rotation
boundary within one pass when the next segment file already exists — and stops at a torn tail
(normal; the writer has not finished yet) or, distinctly, at genuine corruption (bad magic/
version/CRC/length/UTF-8/shape), which is reported rather than silently skipped past — the cursor
never advances beyond a corrupt byte range. Every observation decoded across however many
segments one pass covers is imported in a single `import_batch` call, with `observation_id`s
(UUIDv7) minted by the caller *before* entering the transaction (spec 03 §1.1's identity-minting
discipline — entropy stays out of the write path, the same convention `create_repository`'s own
caller already follows).

As-built note (T13-04, `[SPEC]`, the bounded best-effort window): the two bounds this section's
diagram names — "10 min" and "512 envelopes" — combine as a **union (OR)**, the same
"most-protective" reading `local_rag_store::retention::mark_pins`'s K/T retention window already
established for retiring generations: a best-effort candidate is treated as a duplicate if an
envelope with the same `source_event_id` exists in the same session within the last 512 envelopes
*of that session* **or** within 10 minutes of the new frame's own `captured_at` (not the wall
clock — nothing in the importer reads the system time). "Last 512 of the session" is a
session-scoped rank, not a raw `received_seq` range, because `received_seq` is one global
sequence shared by every session's envelopes.

## 6. Recovery & checkpoints `[FIXED]`

- Consolidation checkpoint on `Stop` and on queue size threshold; best-effort on `SessionEnd`;
  catch-up of unprocessed observations at daemon startup; a background worker owns all of it.
- Spool of sessions absent for > `[SPEC 14 days]` with fully committed cursors → directory GC.
- Import is idempotent under daemon kill at any point: cursor advances only in the same tx as
  the envelopes; re-reading a segment re-skips imported frames via dedup + offset.

As-built note (T13-04, `[SPEC]`): "truncate/delete a segment only after its bytes are ≤ committed
cursor" is implemented by `import_session_tail` as a best-effort filesystem step **after** the
importing transaction commits, deliberately not atomic with it (matching this section's own S4
row below: a daemon kill between commit and truncation just means the next pass's cursor read
re-derives the same "which segments are now fully behind me" answer and deletes them then — never
a correctness dependency). A segment file is deleted as a whole once the cursor's `segment_seq`
has moved past it; the *current* segment is never truncated or deleted in place, even once every
byte up to `committed_offset` has been consumed, since the writer may still be appending to it.
This is distinct from T13-05's 14-day session-directory GC and payload-TTL sweep, which operate on
a different, much longer timescale over abandoned sessions and expired rows, not on the ordinary
per-import segment cleanup described here.

## 7. Spool kill matrix (acceptance, 14 §3)

| # | Kill point | Expected outcome |
| --- | --- | --- |
| S1 | hook killed mid-write (torn frame) | event not durable; importer ignores tail; next hook appends after flock — segment remains valid |
| S2 | hook killed after fdatasync, before exit | event durable, imported once |
| S3 | daemon killed after reading frames, before tx commit | cursor not advanced; re-import; UNIQUE/dedup window prevents duplicates |
| S4 | daemon killed after tx commit, before segment truncation | re-scan skips frames ≤ committed offset |
| S5 | daemon killed mid-truncation/rotation | segment set still consistent (delete-after-commit only) |
| S6 | duplicate stable event across segments (hook retry) | exactly one envelope (UNIQUE dedup_key) |
| S7 | duplicate best-effort event within window | one envelope (windowed dedup); outside window: two envelopes, consolidation idempotence still holds |
| S8 | crash at any point ⇒ | **no event with stable identity is ever lost after spool append** `[FIXED gate]` |
