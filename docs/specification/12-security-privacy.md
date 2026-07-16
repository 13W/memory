# 12 — Security & Privacy

Threat model: local single-user trust domain; adversarial *content* (repo files, recalled
memory, transcripts) rather than adversarial local users. Shared machines/containers are
handled by permissions (§6).

## 1. Data policy `[FIXED]`

`router.data_policy ∈ local_only | metadata_only_remote | allow_remote_with_redaction |
allow_remote_full`, default **`local_only`**. Effective policy = most restrictive of global
and repository settings (02 §3.2). Enforced centrally in the provider pool before provider
selection; violations return `POLICY_BLOCKED_REMOTE`, never silently downgrade.

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

## 3. Retention `[FIXED]`

- `observation_payload` under real TTL (`payload_ttl_hours`), enforced by a sweeper; envelopes
  are durable; `memory_evidence` FKs target envelopes and therefore survive payload expiry.
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
