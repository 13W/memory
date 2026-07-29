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

## 7. Remote fingerprint `[FIXED]`

Git remote identity: credentials stripped, SSH/HTTPS normalized to a canonical form, only the
**hash** stored (`repository.git_remote_fingerprint`). Remote URL is never the sole repository
identifier.
