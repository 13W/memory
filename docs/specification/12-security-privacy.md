# 12 — Security & Privacy

Threat model: local single-user trust domain; adversarial *content* (repo files, recalled
memory, transcripts) rather than adversarial local users. Shared machines/containers are
handled by permissions (§6).

## 1. Data policy `[FIXED]`

`router.data_policy ∈ local_only | metadata_only_remote | allow_remote_with_redaction |
allow_remote_full`, default **`local_only`**. Effective policy = most restrictive of global
and repository settings (02 §3.2). Enforced centrally in the provider pool before provider
selection; violations return `POLICY_BLOCKED_REMOTE`, never silently downgrade.

As-built note (T11-06, `[SPEC]`). **Downloading model assets is not subject to `data_policy`.** The
guard above governs repository content *leaving* the machine — that is what "before provider
selection" is about. Fetching weights is the opposite direction: an explicit user command
(`init --download-models`, 10 §5) pulls public bytes **in**, and no user content is sent. Gating it
on the policy would make a `local_only` installation — the default — unable to obtain the local
model at all, inverting the policy's intent. The download is still constrained: the source URL and
every file's `sha256`/`size` are pinned in the binary (ADR-0005), so "fetch a model" cannot become
"fetch arbitrary bytes". Embedding *through* a remote provider remains gated exactly as fixed above.

As-built note (T16-01, `[SPEC]`). The guard (`local_rag_embed::policy::allows`, T11-03) is now
wired to the real *effective* policy at both call sites that select a provider —
`local_rag_memory::router::route` folds `local_rag_store::effective_data_policy` over a
consolidation window's own involved repositories before calling the generator pool;
`cli::index::project_generation` folds it over the worktree being indexed before calling the
embed pool — not just the raw global `[models] data_policy` value, so a repository's stricter
stored setting now has a real, observable effect end to end (previously computed correctly in
isolation, T02-05, but never actually consulted by production code). The three non-`local_only`
levels are now behaviorally distinct, each in `ProviderPool::embed`/`GeneratorPool::generate`:
`allow_remote_full` sends the original text/messages unchanged; `allow_remote_with_redaction` runs
them through the same `Scanner::redact` (§2) before a remote provider ever sees them.
`metadata_only_remote` is a **pragmatic as-built decision**, not a spec reading: neither of this
workspace's two provider contracts (`Embedder`/`Generator`) has a metadata-only request shape —
both fundamentally need real body text to produce anything useful — so this policy is treated
identically to `local_only` (remote never selected) rather than inventing a lossy
placeholder-payload mode nothing asks for. No real remote provider exists to gate in v0 (D-008);
this task proves the guard's correctness against a fake/test remote provider
(`crates/embed/tests/{policy,gen_policy}.rs`), the same "mechanism real, remote unreachable until a
provider is registered" shape D-026 already documented for `POLICY_BLOCKED_REMOTE` itself.

## 2. Redaction & caps `[FIXED]`

- Secret redaction runs **before** anything is written to the spool `[FIXED]` and again before
  any remote transmission. Scanner: rule set (key/token/password patterns, high-entropy
  strings, known credential formats), versioned `redaction_version` recorded in envelopes
  `[SPEC]`.
- Size caps everywhere content flows (spool payload 256 KiB, evidence excerpt 4 KiB, snippet
  8 KiB `[SPEC]`); truncation always leaves `{hash, original_size}` metadata `[FIXED]`.
- Excluded paths/tools: configurable deny-list; matching events are captured envelope-only
  (identity + metadata, no payload) `[SPEC]`.
- Files classified `secret` by the scanner are `skipped_file(reason='secret')` — no
  `source_blob`, no occurrences (06 §2.2).

As-built note (T12-04, `[SPEC]`): the snippet half of the caps rule is implemented —
`local_rag_search::SNIPPET_CAP_BYTES = 8 * 1024`, with truncation leaving
`{hash, original_size}` over the **full** pre-truncation excerpt via the new
`local_rag_core::identity::Domain::TruncatedExcerpt` (03 §1.2). Its own domain rather than a
reuse of `file_content`: an excerpt is a *slice* of a file, and a snippet that happened to equal
a whole small file would otherwise hash identically to that file's `content_hash` — exactly the
confusion domain separation exists to prevent. The same domain is what memory evidence's 4 KiB
cap should use in group 14. The 256 KiB spool-payload cap remains group 13's.

As-built note (T12-04, `[SPEC]`): the snippet half of the caps rule is implemented —
`local_rag_search::SNIPPET_CAP_BYTES = 8 * 1024`, with truncation leaving
`{hash, original_size}` over the **full** pre-truncation excerpt via the new
`local_rag_core::identity::Domain::TruncatedExcerpt` (03 §1.2). Its own domain rather than a
reuse of `file_content`: an excerpt is a *slice* of a file, and a snippet that happened to equal
a whole small file would otherwise hash identically to that file's `content_hash` — exactly the
confusion domain separation exists to prevent. The same domain is what memory evidence's 4 KiB
cap should use in group 14. The 256 KiB spool-payload cap remains group 13's.

As-built note (T13-01, `[SPEC]`). **Masking transform**: `Scanner::redact` (`crates/core/src/
redaction/mod.rs`) is the payload-rewriting half this module's own doc comment anticipated —
every finding's span replaced with the fixed marker `REDACTION_MARKER = "[REDACTED]"`.
Overlapping/touching findings (a long assigned quoted value that is *also* high-entropy matches
both `AssignedSecret` and `HighEntropy` on the identical span — a real, reachable case, not a
theoretical one) are merged into one replaced range first, so the marker is inserted exactly once
per secret. **256 KiB spool-payload cap**: `local_rag_hook::payload::prepare_payload` caps the
*redacted* bytes at `PAYLOAD_CAP_BYTES = 256 * 1024`, following the identical idiom T12-04
established for the 8 KiB snippet cap — walk back to the nearest UTF-8 boundary (≤3 bytes), then
`{hash, original_size}` via `Domain::TruncatedExcerpt` over the **full** (redacted, pre-cap)
bytes. A payload capped mid-structure is not guaranteed to still be valid JSON, the same way a
capped snippet is not guaranteed syntactically complete — documented, not a defect. **Deny-list**:
`local_rag_core::config::SpoolConfig` (`deny_paths`/`deny_tools`, 02 §3.1's as-built note has the
matching semantics); a denied event's payload is never scanned at all, only its envelope survives.
**A known, accepted limitation**: the scanner runs over the payload as flat text (the same idiom
file classification already uses), not a `serde_json::Value` walk, so the `AssignedSecret` rule
(which expects a bare `"`/`'` immediately after `key =`) is weaker inside a JSON-escaped value
(`\"…\"`); the token-boundary `CredentialToken`/`HighEntropy` rules — the two this section's own
"credential/high-entropy patterns" phrasing names — are unaffected by escaping. Reshaping the
scanner into a JSON-aware transform would expand T03-02's already-gated scope rather than reuse
it, so this is accepted and documented rather than fixed. The **4 KiB evidence-excerpt cap**
remains group 14's, unchanged by this task.

As-built note (D-019, `[SPEC]`, found at gate G13): **"versioned `redaction_version` recorded in
envelopes"** was computed by `prepare_payload` (above) at T13-01 but never actually reached an
envelope — `payload_field` (T13-02) extracted only the redacted bytes, and neither `FramePayload`
(07 §3) nor `observation_envelope` (03 §2.5) carried a field/column for it. Closed end to end:
`local_rag_hook::payload::redaction_version_field` folds it into the frame, `FramePayload.
redaction_version` carries it over the wire (07 §3's own D-019 note), and migration 8
(`observation_envelope.redaction_version`, 03 §2.5's own D-019 note) persists it — `None`/`NULL`
for an envelope-only (denied) event, whose payload this scanner never touches.

As-built note (D-021, `[SPEC]`, found at gate G14): the **4 KiB evidence-excerpt cap** — left to
group 14 by this section's own note above, and never picked up by any T14-0N card — is now
implemented, following the identical idiom `PAYLOAD_CAP_BYTES` established: `EXCERPT_CAP_BYTES =
4 * 1024` in `local_rag_hook::payload`, walked back to a UTF-8 boundary over the same
already-redacted bytes the payload cap uses (not a second scan), with no `{hash, original_size}`
sidecar of its own — the excerpt is a secondary, prompt-facing slice of already-tracked content,
not the authoritative capture. `short_evidence_excerpt_field` populates `FramePayload
.short_evidence_excerpt` (07 §3's own D-021 note) at write time; the field round-trips through
import to `observation_envelope.short_evidence_excerpt` (03 §2.5) unchanged.

**Scanner rule set v0 (as-built, T03-02) `[SPEC]`.** The scanner is a single shared component
(`local-rag-core::redaction`, `redaction_version = 1`) reused by file classification, spool
ingestion, and remote transmission so verdicts stay consistent and auditable against one version.
Detection is deterministic, dependency-free byte/line scanning (no regex engine): (1) PEM
private-key header lines; (2) known credential formats by prefix + plausible length (AWS
`AKIA`/`ASIA`, GitHub `ghp_`/`gho_`/`ghs_`/`github_pat_`, Slack `xox[bpas]-`, OpenAI-style `sk-`);
(3) secret-like keys (`password`/`secret`/`api_key`/`token`/…) assigned a **quoted** literal of
non-trivial length (unquoted assignments in ordinary code are not flagged); (4) long high-entropy
strings (≥ 40 chars, Shannon entropy ≥ 4.5 bits/char, above a hex digest's 4.0 ceiling so git SHAs
do not trip). The set is intentionally conservative and expected to grow; any change bumps
`redaction_version`. The classifier consumes a boolean verdict; scan spans (for payload redaction
in spool/remote flows) are exposed for later groups.

## 3. Retention `[FIXED]`

- `observation_payload` under real TTL (`payload_ttl_hours`), enforced by a sweeper; envelopes
  are durable; `memory_evidence` FKs target envelopes and therefore survive payload expiry.

As-built note (T13-04, `[SPEC]`): `payload_ttl_hours` (`StorageConfig::payload_ttl_hours`, default
72h, already introduced by an earlier storage-foundation task) is now a real consumer:
`local_rag_store::observation::import_batch` computes `observation_payload.expires_at =
now_ms + payload_ttl_hours × 3_600_000` at import time, for every observation that has a payload
row at all (an envelope-only/denied event never gets one — its absence *is* "no payload", not an
expired one). The sweeper that actually deletes rows past `expires_at` is T13-05's,
`local_rag_store::observation::run_payload_ttl_sweep`: a single `DELETE FROM observation_payload
WHERE expires_at <= now_ms` per sweep (`<=`, not `<` — the same "a deadline exactly now means
remove now" convention `housekeeping::shard_destroy_due` established), plus a metrics readout
(`payload_removed`/`payload_retained`/`total_envelopes`) alongside it. `observation_envelope` and
`observation_path` are never touched by this sweep — envelope survival past payload expiry is
structural, not a decision this code makes: an envelope with no payload row looks identical
whether it never had one or its payload already expired. Ships with no scheduler, the same
deferral every sweep in this crate carries (triggering it periodically is the daemon's job, group
15).
- `inspect / export / purge` exist as first-class CLI operations (11 §6). `purge` is the only
  hard-delete path and tombstones audit references `[SPEC]`.
- **The author's original text stays visible** `[FIXED, ADR-0011]`. Durable memory is stored in
  English (08 §3), so `inspect` and `export` must show the text as it was written alongside the
  stored canon: an owner reading their own store has to see their own words, not only a machine's
  rendering of them. `purge` removes both — both are the same person's data.
- **Observations and evidence are never translated** `[FIXED, ADR-0011]`. What hooks capture and
  what `give_feedback` records is a record of what was actually said; translating it would turn
  evidence into paraphrase and hollow out `inspect_memory_evidence`. This does not weaken the
  language invariant: the consolidation router writes entry text in English regardless of the
  language of the observations it read, so the durable entry is English anyway
  ([ADR-0011](../adr/0011-english-canon-for-durable-memory.md) §Decision 7).

As-built note (T16-02, `[SPEC]`): implemented as `local_rag_store::privacy::{inspect,export,
purge}` (11 §6's own as-built note has the CLI wiring and flag shape). `purge_memory` deletes the
`memory_entry` row and its `memory_evidence` rows, relinks any descendant whose `supersedes_id`
pointed at it to `NULL` (the FK has no `ON DELETE` clause — SQLite would otherwise refuse the
delete outright), and rewrites its `audit_event` trail: every prior row's `payload` is set to
`NULL`, and a new terminal `op = "purge"` row is appended. `entity_kind`/`entity_id` on
`audit_event` carry no FK (03 §2.5), so a row outliving its `memory_entry` parent — the tombstone
itself — is not a constraint violation. `purge_session` hard-deletes every `observation_envelope`
(cascading `observation_path`/`observation_payload`) for a session, first deleting any
`memory_evidence`/`candidate_evidence` rows that reference those envelopes (same FK reasoning);
this can leave a `memory_entry` or `pending_memory_candidate` with zero evidence rows, an
already-legal state this schema tolerates elsewhere (`propose_candidate` already accepts an empty
evidence set) and not a new orphan class. `audit_event` is never touched by a session purge — this
crate's `entity_kind` values are only `memory_entry`/`candidate`, never `observation_envelope`,
and the only writer of `audit_event.payload` (`apply_merge`) stores loser memory-ids, never raw
observation content. `purge_all` purges every memory entry and every session's observations
**unconditionally, in one transaction** — not batched like the retention sweep (05 §5): a
partially-completed purge is a worse outcome than a slow one for an all-or-nothing privacy/legal
operation. `local_rag_store::retention::ExternalPins.referenced_generations` (06 §5) is not wired
by this task: no column on `memory_entry`/`observation_envelope` carries an actual `generation_id`
reference today, so there is nothing yet for that pin to name.
As-built note (T21-07, `[SPEC]`, ADR-0010): since migration 14 an entry may carry a second copy of
the user's own writing — `memory_text_normalization.normalized_text`, the English variant a local
model produced from it (03 §2.5) — and all three operations above now account for it.

**`purge`**: `purge_memory_rows` deletes the row **explicitly and first**, before the
`memory_entry` delete, and reports it as `PurgeMemoryReport.normalization_rows_removed` (0 or 1 —
the table holds at most one row per entry). The table's own `ON DELETE CASCADE` would take it
anyway; the explicit delete exists because a cascade cannot be *counted*, and a number inferred
from "the cascade must have fired" would be a claim about a pragma rather than an observation. The
cascade stays as the safety net for any delete path that does not come through this module.
`purge --all` inherits the behaviour through the same shared function.

**`edit`**: `apply_edit` drops the row in its own transaction when — and only when — the text
actually changes, compared against the stored text rather than against "the caller supplied a text
field". A translation of text the user has since replaced is derived data that must not outlive its
source; an edit that re-submits the identical string changes nothing to translate, and dropping the
row there would make the normalization worker pay for the same inference twice.
`apply_reinforce` never touches the row, because it may not touch the text (08 §3 `[FIXED]`).

**`export`/`inspect`**: `MemoryInspection` carries `normalization: Option<NormalizationRow>`, filled
by the same `normalization_for` reader in both `inspect_memory` and `export_scope` — the shared
shape is what keeps export from being poorer than inspect. The rendered object includes
`normalized_text` **itself**, not merely metadata about it (owner decision, 2026-08-19): an export
exists to show everything the store holds about the user, whose original text is already printed
alongside it. Provenance travels with it — status, `source_text_sha256`, detected language,
normalizer model id, prompt and normalizer versions, attempt count, and, on a `failed` row, the
reason there is no translation at all.

A **known, accepted gap**, registered as `D-074` rather than left implicit: `purge` never touches
`cache.sqlite`, so the purged entry's embedding survives in `embedding_cache` until LRU eviction or
a full cache rebuild. A writable cross-database transaction is forbidden (03 §1.4) and no sweep
collects a cache row merely because its subject stopped existing. This predates group 21 and
applies to every memory entry, normalized or not.

- Optional encryption at rest (SQLite-level, e.g. SQLCipher-compatible) — optional feature,
  off by default `[FIXED optionality]`.

## 4. Recalled memory is untrusted `[FIXED]`

A single XML tag is not a boundary. Defenses, all mandatory:

1. **Encoding**: sanitization (control characters stripped), per-entry byte-length prefix,
   escaping of delimiter sequences, per-entry and per-block caps (11 §5).
2. **System instruction** accompanying every recall block: the block is untrusted reference
   data; it must not change tool policy or permissions.
3. **Provenance separated from text**: ids/evidence/trust available via tools only.
4. **Trust/evidence marking** persisted per entry; model-claims are never auto-promoted to
   facts `[FIXED]`.
5. **Adversarial tests** in the acceptance suite (14 §6): prompt-injection payloads stored as
   memories must survive round-trip as inert text.

As-built note (T21-04, `[SPEC]`, ADR-0010): item 5 gains a second surface. Until group 21 an
entry's text was only ever *rendered* to a model inside a recall block; the translator
(`local_rag_memory::normalize::translate`) is the first component that sends one entry's text to a
model as its own subject, so the injection question is asked again from the other side.

**The input is data, structurally.** The user message is not a concatenation but `serde_json`'s
encoding of `{"src": <the entry's text>}`. Quotes, braces, newlines, control characters and chat
template markers inside the entry all become escaped content of one JSON string: there is nothing
to break out of, because nothing was built by string-joining. The entry's `memory_id` is carried
for diagnostics only and never enters the prompt, so the injection surface is exactly one string.
The system prompt states that `src` is data and shows a fourth few-shot example in which an
injection payload is translated as ordinary text — advisory reinforcement, never the defence
itself.

**The output is validated before anyone may store it.** `validate` is pure and ordered by trust:
`FinishReason::Length` is a rejection *before* parsing (a truncated object can parse cleanly), the
parse admits exactly one `{"en": …}` object with `deny_unknown_fields`, an empty answer is a
refusal, an answer still in a non-Latin script means the source was echoed rather than translated,
a length outside a sane band of the source is a fragment or an essay, and the byte ceiling rejects
rather than truncates.

**The adversarial set, by name** (`normalize::translate`'s `ADVERSARIAL` table, seven payloads —
each asserted twice: the built message stays one JSON object with exactly one `src` key, and a
"compromised" answer to it is rejected by the validator): *instruction injection*,
*memory-block terminator literal* (`</memory>`), *answer-envelope literal* (`{"en":`), *the entry
is itself json*, *control characters*, *chat template marker* (`<start_of_turn>system`), *demand
to return nothing*.

Every failure is classified before it can be acted on (`classify_translate_failure`), so the
lesson of D-050/D-057 holds here too: a deterministic rejection is `Mechanical`, a missing or
policy-blocked generator is `Unavailable` and marks no entry as failed at all, and only a genuine
infrastructure failure is `Transient`.

As-built note (T16-04, `[FIXED]`): item 5 (GAP-05) is closed end to end, plus the adjacent
malicious-indexed-code/secret/symlink corpus the group-16 card also names — see 14 §6's own
as-built note for the full list of tests, one per property.

As-built note (T14-07, `[SPEC]`): item 4's "model-claims are never auto-promoted to facts" is
enforced twice, independently, on purpose. First, proactively:
`local_rag_memory::guard::materialize` downgrades a router-proposed `create`/`supersede` of kind
`fact | decision | convention | procedure` to `propose_candidate` whenever every cited
observation's `evidence_kind` (set at write time, T13-04 — never the model's own claim) is
`model_claim`. Second, as a backstop that cannot be bypassed by a future generator, a bug in the
router, or a direct `commit_apply_run` call: `local_rag_store::memory::op::apply_create`/
`apply_supersede` reject the identical condition with `MemoryOpError::ModelClaimOnlyProvenance`
before any mutation, whenever `actor == Router` — `actor == User` is exempt by construction
(spec 08 §5's `remember`/candidate-approval path, where a human already vouched for the claim).
`local_rag_store::run_once` is generic over any `generate` closure (08 §4's own as-built note),
so only the second layer is the one this guarantee's correctness actually rests on; the first
exists so the common case never needs the backstop to fire at all.

## 5. Source-blob policy — strict invariant `[FIXED]`

```
no source_blob  ⇒  file is not part of the canonical indexed generation (no occurrences)
```

Binary/LFS/huge/secret/ignored/encoding-unsupported files → `skipped_file` (path, reason,
optional content_hash), **no searchable occurrences**. The `non_rebuildable` tier (reading
live disk at query time) is **rejected for v0** — it breaks the single source of truth.
The explicit tradeoff stands: canonical reproducibility requires a local copy of every
indexed source (zstd compression; retention/backup accounted in metrics) `[FIXED]`.

## 6. Filesystem & endpoint permissions `[FIXED]`

Store dirs 0700, files/segments 0600 (POSIX); endpoint socket dir 0700, socket 0600; Windows
named pipe with owner-only ACL; per-user store paths (02 §2.1). Shared machine/container: the
store MUST NOT be shared between OS users; the daemon refuses to start on a store whose owner
uid differs `[SPEC]`.

As-built note (D-027, `[SPEC]`, found at T16-03 by its own new permissions-audit section).
`state.sqlite`/`cache.sqlite` were the two most frequently opened files in the store yet were the
only managed files never wrapped in `local_rag_core::paths::ensure_file_0600` — `open_state_rw`/
`open_cache_rw` (`crates/store/src/{state,cache}/open.rs`) called a bare `Connection::open(path)`,
so a freshly created file landed at the process umask's default (typically `0644`), not `0600`,
pre-existing since T01-02/T01-05. Both functions now call `ensure_file_0600(path)` first, the
same idiom `store.lock`/spool segments/migration backups/the migration lock already used —
idempotent: `0600` on first creation, owner-verified and re-asserted on every subsequent open.

As-built note (T16-04, `[SPEC]`). "The daemon refuses to start on a store whose owner uid
differs" now has a dedicated integration test, `crates/local-rag/tests/lifecycle_startup.rs::
a_wrong_owner_store_refuses_startup` — platform-gated (`chown` to a different uid only succeeds
under root; the attempt itself is the gate) and provably safe regardless of privilege
(`perms::ensure_dir`'s owner check runs strictly before its only write, and only a
`TempHome`-isolated directory is ever touched, never a real system path). `DaemonHandle::start`'s
own `layout.ensure()` failure now surfaces as a typed `DaemonStartupError::Path(PathError)`
(`crates/local-rag/src/daemon/lifecycle.rs`), replacing a prior lossy `Bind(io::Error::other(...))`
wrap that wasted the already-computed `PathError::WrongOwner` structure.

## 7. Remote fingerprint `[FIXED]`

Git remote identity: credentials stripped, SSH/HTTPS normalized to a canonical form, only the
**hash** stored (`repository.git_remote_fingerprint`). Remote URL is never the sole repository
identifier.
