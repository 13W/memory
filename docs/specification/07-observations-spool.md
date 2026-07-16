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

## 6. Recovery & checkpoints `[FIXED]`

- Consolidation checkpoint on `Stop` and on queue size threshold; best-effort on `SessionEnd`;
  catch-up of unprocessed observations at daemon startup; a background worker owns all of it.
- Spool of sessions absent for > `[SPEC 14 days]` with fully committed cursors → directory GC.
- Import is idempotent under daemon kill at any point: cursor advances only in the same tx as
  the envelopes; re-reading a segment re-skips imported frames via dedup + offset.

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
