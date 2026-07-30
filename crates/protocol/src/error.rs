//! The canonical daemon-protocol error/degraded vocabulary (spec 02 §6) — T09-03.
//!
//! Shared by every daemon subsystem (code search, memory, ...), not just code
//! search — hence living here rather than in `local-rag-search`. Variants are
//! added by the task that first *detects* the condition, never ahead of it
//! (that would be unused, undead code): T09-03's search skeleton defined the
//! first three, and T11-03's embedding provider pool added
//! `POLICY_BLOCKED_REMOTE` — it is the central remote-policy guard (spec 12 §1,
//! 10 §1) and therefore the first code that can refuse a call on policy
//! grounds. T15-01's daemon startup/lifecycle (spec 02 §4.1) is the first code
//! that can detect spec 02 §6's remaining three rows — `MigrationInProgress`,
//! `StoreLocked`, `IncompatibleStore` — added here for that reason.
//! `IncompatibleStore`'s `details` is filled in by the caller from
//! `local_rag_store::migrate::MigrationError`'s already-typed variants
//! (`IncompatibleStore`/`ChecksumDrift`/`UnknownAppliedVersion`); this crate
//! never gains a `store` dependency to re-derive that condition itself
//! (`local-rag-protocol` "depends on nothing but `core`", see below).
//!
//! [`ErrorEnvelope`] is the wire shape `{code, message, retryable, details?}`;
//! `details` stays a freeform `Option<String>` (spec marks its shape
//! unfixed).
//!
//! As-built note (T15-03, `[SPEC]`): the JSON mapping this module's own doc
//! once deferred to "group 15's concern" is now wired — [`ErrorCode`] has a
//! hand-written `Serialize` delegating to [`ErrorCode::as_str`] (the same
//! precedent [`crate::search::Snippet`]'s hand-written `Serialize` sets,
//! rather than a derive with a `#[serde(rename)]` per variant, so the wire
//! string stays single-sourced through one function instead of being spelled
//! twice), and [`ErrorEnvelope`] derives `Serialize` with `details` omitted
//! entirely when absent (`skip_serializing_if`, spec 09 §7's "absent is
//! absent, not null" rule applied here too). Field order is declaration
//! order — `{code, message, retryable, details?}`, literally this section's
//! own spelling. `local-rag`'s `daemon::mcp` is the first (and, this task's
//! own scope, only) consumer: it wraps this JSON as MCP `isError` content
//! text (spec 02 §6: "MCP tools map `code` into `isError` content with the
//! same code string").

use std::fmt;

/// A canonical daemon-protocol error code (spec 02 §6 error taxonomy table).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Both the lexical and dense legs are unavailable (spec 02 §6).
    IndexUnavailable,
    /// The request's worktree is unknown or has never been indexed (spec 02 §6).
    WorktreeNotIndexed,
    /// A generation/model-space switch is in flight and the bounded wait on
    /// `L2.read` elapsed (spec 02 §6).
    BusyRetry,
    /// The effective `data_policy` forbids the remote call the operation would
    /// need (spec 02 §6, 12 §1). Refused, never silently downgraded to a
    /// weaker policy or to a different provider class.
    PolicyBlockedRemote,
    /// The requested search mode is recognized but not supported in v0
    /// (spec 09 §5: `semantic` is the description leg, post-v0 and
    /// benchmark-gated `[FIXED]`).
    UnsupportedMode,
    /// The requested path is not part of the worktree's active generation —
    /// either never seen, or deliberately skipped (spec 06 §2.2). `details`
    /// says which.
    PathNotIndexed,
    /// A schema migration is required or currently running; the daemon serves
    /// only health/status until it finishes (spec 02 §6, 13 §3 "the daemon
    /// otherwise quiescent").
    MigrationInProgress,
    /// The store is exclusively held by another live daemon instance (spec 02
    /// §4.1 startup, §6). `details` names the owning instance.
    StoreLocked,
    /// The store's schema is not usable by this binary: newer than supported,
    /// a migration checksum drift, or rewritten migration history (spec 02
    /// §6, 13 §3). `details` disambiguates which.
    IncompatibleStore,
    /// A memory-write tool named a `memory_id` with no `memory_entry` row
    /// (spec 08 §3).
    UnknownMemory,
    /// `expected_version` did not match the entry's current `entry_version`
    /// (spec 08 §3's optimistic-concurrency precondition). `details` names
    /// both values.
    OptimisticConflict,
    /// `create`'s (or `supersede`'s new-entry) `canonical_key` already
    /// exists in the same scope (spec 08 §3).
    CanonicalKeyConflict,
    /// `scope_kind='global'` with a `scope_owner_id` other than the fixed
    /// singleton (spec 03 §2.5).
    InvalidGlobalScopeOwner,
    /// The entry's kind-specific state machine forbids the requested
    /// transition (spec 04 §5). `details` names the kind/from/to.
    IllegalMemoryTransition,
    /// `edit_memory` targets an entry whose current state is terminal (spec
    /// 08 §3's as-built `edit` guard).
    EntryTerminal,
    /// `merge_memories`: a loser's `(scope_kind, scope_owner_id)` differs
    /// from the survivor's (spec 08 §3).
    IncompatibleScope,
    /// `merge_memories` was called with no losers (spec 08 §3: "losers must
    /// be non-empty").
    EmptyMergeSet,
    /// The model-claims-are-never-auto-promoted-to-facts backstop refused a
    /// router-actor `create`/`supersede` of a promotion kind backed only by
    /// `model_claim` evidence (spec 12 §4).
    ModelClaimOnlyProvenance,
    /// A candidate-review tool named a `candidate_id` with no
    /// `pending_memory_candidate` row (spec 04 §6).
    UnknownCandidate,
    /// The candidate review-state machine forbids the requested transition
    /// (spec 04 §6). `details` names the from/to.
    IllegalCandidateTransition,
    /// `edit_memory_candidate` targets a candidate whose `review_state` is
    /// no longer `pending` (spec 04 §6 — candidates have no version to check
    /// instead).
    CandidateNotPending,
    /// A candidate's `proposed_operation` failed to parse, or named a
    /// `kind`/`scope_kind` outside the CHECK domain (spec 04 §6).
    InvalidProposedOperation,
}

impl ErrorCode {
    /// The wire-format code string (spec 02 §6's `Flag / error` column).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::IndexUnavailable => "INDEX_UNAVAILABLE",
            ErrorCode::WorktreeNotIndexed => "WORKTREE_NOT_INDEXED",
            ErrorCode::BusyRetry => "BUSY_RETRY",
            ErrorCode::PolicyBlockedRemote => "POLICY_BLOCKED_REMOTE",
            ErrorCode::UnsupportedMode => "UNSUPPORTED_MODE",
            ErrorCode::PathNotIndexed => "PATH_NOT_INDEXED",
            ErrorCode::MigrationInProgress => "MIGRATION_IN_PROGRESS",
            ErrorCode::StoreLocked => "STORE_LOCKED",
            ErrorCode::IncompatibleStore => "INCOMPATIBLE_STORE",
            ErrorCode::UnknownMemory => "UNKNOWN_MEMORY",
            ErrorCode::OptimisticConflict => "OPTIMISTIC_CONFLICT",
            ErrorCode::CanonicalKeyConflict => "CANONICAL_KEY_CONFLICT",
            ErrorCode::InvalidGlobalScopeOwner => "INVALID_GLOBAL_SCOPE_OWNER",
            ErrorCode::IllegalMemoryTransition => "ILLEGAL_MEMORY_TRANSITION",
            ErrorCode::EntryTerminal => "ENTRY_TERMINAL",
            ErrorCode::IncompatibleScope => "INCOMPATIBLE_SCOPE",
            ErrorCode::EmptyMergeSet => "EMPTY_MERGE_SET",
            ErrorCode::ModelClaimOnlyProvenance => "MODEL_CLAIM_ONLY_PROVENANCE",
            ErrorCode::UnknownCandidate => "UNKNOWN_CANDIDATE",
            ErrorCode::IllegalCandidateTransition => "ILLEGAL_CANDIDATE_TRANSITION",
            ErrorCode::CandidateNotPending => "CANDIDATE_NOT_PENDING",
            ErrorCode::InvalidProposedOperation => "INVALID_PROPOSED_OPERATION",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for ErrorCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// The canonical daemon protocol error envelope (spec 02 §6:
/// `{code, message, retryable: bool, details?}`). MCP tools map `code` into
/// `isError` content with the same code string.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ErrorEnvelope {
    /// The canonical error code.
    pub code: ErrorCode,
    /// A human-readable message.
    pub message: String,
    /// Whether the caller should retry (only [`ErrorCode::BusyRetry`] today).
    pub retryable: bool,
    /// Freeform diagnostic detail (spec 02 §6: "every degraded search response
    /// includes the validation reason"). Omitted from the wire form entirely
    /// when absent — spec 09 §7's "absent is absent, not null" rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl ErrorEnvelope {
    /// Both legs unavailable (spec 02 §6). Not retryable: a caller retrying
    /// immediately would observe the same unavailability.
    pub fn index_unavailable(details: impl Into<String>) -> Self {
        ErrorEnvelope {
            code: ErrorCode::IndexUnavailable,
            message: "both the lexical and dense legs are unavailable".to_string(),
            retryable: false,
            details: Some(details.into()),
        }
    }

    /// The request's worktree is unknown / never indexed (spec 02 §6).
    pub fn worktree_not_indexed() -> Self {
        ErrorEnvelope {
            code: ErrorCode::WorktreeNotIndexed,
            message: "worktree is unknown or has never been indexed".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// The requested search mode is not supported in v0 (spec 09 §5). Not
    /// retryable: `semantic` becomes available only if the description leg
    /// wins the benchmark, which no retry can bring about.
    pub fn unsupported_mode(mode: impl fmt::Display) -> Self {
        ErrorEnvelope {
            code: ErrorCode::UnsupportedMode,
            message: format!("search mode {mode} is not supported in v0"),
            retryable: false,
            details: None,
        }
    }

    /// The path is absent from the active generation (spec 06 §2.2). Not
    /// retryable: the same path in the same generation is absent identically.
    /// `details` distinguishes "skipped, reason=…" from "no such path", because
    /// those are different answers to the caller.
    pub fn path_not_indexed(path: &str, details: impl Into<String>) -> Self {
        ErrorEnvelope {
            code: ErrorCode::PathNotIndexed,
            message: format!("path {path:?} is not part of the active generation"),
            retryable: false,
            details: Some(details.into()),
        }
    }

    /// The bounded wait on `L2.read` elapsed while a writer held `L2.write`
    /// (spec 02 §6). Retryable: the in-flight switch is expected to finish.
    pub fn busy_retry() -> Self {
        ErrorEnvelope {
            code: ErrorCode::BusyRetry,
            message: "a generation/model-space switch is in flight; retry shortly".to_string(),
            retryable: true,
            details: None,
        }
    }

    /// A schema migration is required or in progress; only health/status is
    /// served (spec 02 §6, 13 §3). Retryable: the identical request, once the
    /// migration finishes, succeeds.
    pub fn migration_in_progress() -> Self {
        ErrorEnvelope {
            code: ErrorCode::MigrationInProgress,
            message: "a schema migration is required or in progress; only health/status \
                      is served until it finishes"
                .to_string(),
            retryable: true,
            details: None,
        }
    }

    /// The store is exclusively held by another live daemon instance (spec 02
    /// §4.1). Not retryable: this instance refuses to start against a store
    /// another instance owns; `details` names the owner so the caller can
    /// decide whether to wait for it or investigate.
    pub fn store_locked(owner_pid: u32, owner_instance_uuid: &str) -> Self {
        ErrorEnvelope {
            code: ErrorCode::StoreLocked,
            message: "the store is locked by another running daemon instance".to_string(),
            retryable: false,
            details: Some(format!(
                "owner pid={owner_pid} instance_uuid={owner_instance_uuid}"
            )),
        }
    }

    /// The store's schema is not usable by this binary (spec 02 §6, 13 §3):
    /// newer than supported, a migration checksum drift, or rewritten
    /// migration history. Not retryable: the binary must be upgraded (or the
    /// store restored) before the same request can succeed. `details`
    /// disambiguates which condition — e.g. `"store_version 3 > binary_max
    /// 2"` or `"checksum drift at version 1"` (spec 02 §6's own examples).
    pub fn incompatible_store(details: impl Into<String>) -> Self {
        ErrorEnvelope {
            code: ErrorCode::IncompatibleStore,
            message: "the store's schema is not usable by this binary".to_string(),
            retryable: false,
            details: Some(details.into()),
        }
    }

    /// No `memory_entry` row with this id (spec 08 §3). Not retryable: the
    /// same id names nothing until a separate `create` succeeds.
    pub fn unknown_memory() -> Self {
        ErrorEnvelope {
            code: ErrorCode::UnknownMemory,
            message: "no memory entry with this id".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// `expected_version` did not match the entry's current `entry_version`
    /// (spec 08 §3). Not retryable *as-is*: the caller must re-read the
    /// current version before retrying, not simply resend the same request.
    pub fn optimistic_conflict(expected: i64, actual: i64) -> Self {
        ErrorEnvelope {
            code: ErrorCode::OptimisticConflict,
            message: "expected_version does not match the entry's current version".to_string(),
            retryable: false,
            details: Some(format!("expected entry_version {expected}, found {actual}")),
        }
    }

    /// `canonical_key` already exists in this scope (spec 08 §3). Not
    /// retryable: the same key in the same scope conflicts identically.
    pub fn canonical_key_conflict() -> Self {
        ErrorEnvelope {
            code: ErrorCode::CanonicalKeyConflict,
            message: "canonical_key already exists in this scope".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// `scope_kind='global'` with a `scope_owner_id` other than the fixed
    /// singleton (spec 03 §2.5). Not retryable: a caller-shape error.
    pub fn invalid_global_scope_owner() -> Self {
        ErrorEnvelope {
            code: ErrorCode::InvalidGlobalScopeOwner,
            message: "global scope requires the fixed singleton scope_owner_id".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// The entry's kind-specific state machine forbids the requested
    /// transition (spec 04 §5). Not retryable: the same transition is
    /// illegal for this kind regardless of when it is retried.
    pub fn illegal_memory_transition(details: impl Into<String>) -> Self {
        ErrorEnvelope {
            code: ErrorCode::IllegalMemoryTransition,
            message: "this transition is illegal for the entry's kind".to_string(),
            retryable: false,
            details: Some(details.into()),
        }
    }

    /// `edit_memory` targets an entry whose current state is terminal (spec
    /// 08 §3's as-built `edit` guard). Not retryable: a terminal state never
    /// becomes editable again.
    pub fn entry_terminal() -> Self {
        ErrorEnvelope {
            code: ErrorCode::EntryTerminal,
            message: "the entry's current state is terminal".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// `merge_memories`: a loser's scope differs from the survivor's (spec
    /// 08 §3). Not retryable: the same scope mismatch persists.
    pub fn incompatible_scope() -> Self {
        ErrorEnvelope {
            code: ErrorCode::IncompatibleScope,
            message: "every merged entry must share the survivor's scope".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// `merge_memories` was called with no losers (spec 08 §3). Not
    /// retryable: a caller-shape error.
    pub fn empty_merge_set() -> Self {
        ErrorEnvelope {
            code: ErrorCode::EmptyMergeSet,
            message: "merge requires at least one loser".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// The model-claims-never-auto-promoted-to-facts backstop refused a
    /// router-actor `create`/`supersede` backed only by `model_claim`
    /// evidence (spec 12 §4). Not retryable: only new, non-model-claim
    /// evidence changes this outcome.
    pub fn model_claim_only_provenance() -> Self {
        ErrorEnvelope {
            code: ErrorCode::ModelClaimOnlyProvenance,
            message: "model-claim-only evidence cannot promote to this kind".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// No `pending_memory_candidate` row with this id (spec 04 §6). Not
    /// retryable.
    pub fn unknown_candidate() -> Self {
        ErrorEnvelope {
            code: ErrorCode::UnknownCandidate,
            message: "no candidate with this id".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// The candidate review-state machine forbids the requested transition
    /// (spec 04 §6). Not retryable.
    pub fn illegal_candidate_transition(details: impl Into<String>) -> Self {
        ErrorEnvelope {
            code: ErrorCode::IllegalCandidateTransition,
            message: "this candidate review-state transition is illegal".to_string(),
            retryable: false,
            details: Some(details.into()),
        }
    }

    /// `edit_memory_candidate` targets a candidate whose `review_state` is
    /// no longer `pending` (spec 04 §6). Not retryable: pending never
    /// returns once left.
    pub fn candidate_not_pending() -> Self {
        ErrorEnvelope {
            code: ErrorCode::CandidateNotPending,
            message: "the candidate is no longer pending".to_string(),
            retryable: false,
            details: None,
        }
    }

    /// A candidate's `proposed_operation` failed to parse, or named a
    /// `kind`/`scope_kind` outside the CHECK domain (spec 04 §6). Not
    /// retryable: the stored payload does not change on retry.
    pub fn invalid_proposed_operation(details: impl Into<String>) -> Self {
        ErrorEnvelope {
            code: ErrorCode::InvalidProposedOperation,
            message: "proposed_operation failed to parse".to_string(),
            retryable: false,
            details: Some(details.into()),
        }
    }
}

/// Spec 09 §7's `degraded` field vocabulary (`null | "dense_only" |
/// "lexical_only"` — the `null` case is `Option<DegradedMode>::None` at the
/// call site).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedMode {
    /// The FTS view is invalid/stale; search served dense-only (spec 02 §6).
    DenseOnly,
    /// The dense shard is unavailable/rebuilding; search served lexical-only
    /// (spec 02 §6).
    LexicalOnly,
}

impl DegradedMode {
    /// The wire-format string (spec 09 §7).
    pub fn as_str(self) -> &'static str {
        match self {
            DegradedMode::DenseOnly => "dense_only",
            DegradedMode::LexicalOnly => "lexical_only",
        }
    }
}

impl fmt::Display for DegradedMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_strings_match_spec_02_section_6() {
        assert_eq!(ErrorCode::IndexUnavailable.as_str(), "INDEX_UNAVAILABLE");
        assert_eq!(
            ErrorCode::WorktreeNotIndexed.as_str(),
            "WORKTREE_NOT_INDEXED"
        );
        assert_eq!(ErrorCode::BusyRetry.as_str(), "BUSY_RETRY");
        assert_eq!(
            ErrorCode::PolicyBlockedRemote.as_str(),
            "POLICY_BLOCKED_REMOTE"
        );
        assert_eq!(ErrorCode::UnsupportedMode.as_str(), "UNSUPPORTED_MODE");
        assert_eq!(ErrorCode::PathNotIndexed.as_str(), "PATH_NOT_INDEXED");
        assert_eq!(
            ErrorCode::MigrationInProgress.as_str(),
            "MIGRATION_IN_PROGRESS"
        );
        assert_eq!(ErrorCode::StoreLocked.as_str(), "STORE_LOCKED");
        assert_eq!(ErrorCode::IncompatibleStore.as_str(), "INCOMPATIBLE_STORE");
    }

    #[test]
    fn unsupported_mode_names_the_mode_and_is_not_retryable() {
        let err = ErrorEnvelope::unsupported_mode(crate::SearchMode::Semantic);
        assert_eq!(err.code, ErrorCode::UnsupportedMode);
        assert!(!err.retryable);
        assert!(err.message.contains("semantic"), "{}", err.message);
    }

    #[test]
    fn degraded_mode_strings_match_spec_09_section_7() {
        assert_eq!(DegradedMode::DenseOnly.as_str(), "dense_only");
        assert_eq!(DegradedMode::LexicalOnly.as_str(), "lexical_only");
    }

    #[test]
    fn only_busy_retry_and_migration_in_progress_are_retryable() {
        assert!(!ErrorEnvelope::index_unavailable("both legs down").retryable);
        assert!(!ErrorEnvelope::worktree_not_indexed().retryable);
        assert!(ErrorEnvelope::busy_retry().retryable);
        assert!(ErrorEnvelope::migration_in_progress().retryable);
        assert!(!ErrorEnvelope::store_locked(123, "uuid-a").retryable);
        assert!(!ErrorEnvelope::incompatible_store("x").retryable);
    }

    #[test]
    fn store_locked_names_the_owner_in_details() {
        let env = ErrorEnvelope::store_locked(4242, "instance-uuid-x");
        assert_eq!(env.code, ErrorCode::StoreLocked);
        let details = env.details.expect("details");
        assert!(details.contains("4242"), "{details}");
        assert!(details.contains("instance-uuid-x"), "{details}");
    }

    #[test]
    fn incompatible_store_carries_the_caller_supplied_details() {
        let env = ErrorEnvelope::incompatible_store("store_version 3 > binary_max 2");
        assert_eq!(env.code, ErrorCode::IncompatibleStore);
        assert_eq!(
            env.details.as_deref(),
            Some("store_version 3 > binary_max 2")
        );
    }

    #[test]
    fn migration_in_progress_carries_no_details() {
        let env = ErrorEnvelope::migration_in_progress();
        assert_eq!(env.code, ErrorCode::MigrationInProgress);
        assert_eq!(env.details, None);
    }

    #[test]
    fn constructors_set_the_matching_code() {
        assert_eq!(
            ErrorEnvelope::index_unavailable("x").code,
            ErrorCode::IndexUnavailable
        );
        assert_eq!(
            ErrorEnvelope::worktree_not_indexed().code,
            ErrorCode::WorktreeNotIndexed
        );
        assert_eq!(ErrorEnvelope::busy_retry().code, ErrorCode::BusyRetry);
    }

    #[test]
    fn index_unavailable_carries_the_caller_supplied_details() {
        let env = ErrorEnvelope::index_unavailable("fts: head missing; dense: shard corrupt");
        assert_eq!(
            env.details.as_deref(),
            Some("fts: head missing; dense: shard corrupt")
        );
    }

    #[test]
    fn error_code_serializes_as_its_wire_string() {
        assert_eq!(
            serde_json::to_string(&ErrorCode::WorktreeNotIndexed).unwrap(),
            "\"WORKTREE_NOT_INDEXED\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorCode::BusyRetry).unwrap(),
            "\"BUSY_RETRY\""
        );
    }

    #[test]
    fn envelope_serializes_in_declaration_order_with_details_present() {
        let env = ErrorEnvelope::store_locked(4242, "instance-uuid-x");
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            "{\"code\":\"STORE_LOCKED\",\"message\":\"the store is locked by another running \
             daemon instance\",\"retryable\":false,\"details\":\"owner pid=4242 \
             instance_uuid=instance-uuid-x\"}"
        );
    }

    #[test]
    fn absent_details_is_omitted_not_nulled() {
        let env = ErrorEnvelope::worktree_not_indexed();
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            "{\"code\":\"WORKTREE_NOT_INDEXED\",\"message\":\"worktree is unknown or has never \
             been indexed\",\"retryable\":false}"
        );
        assert!(!serde_json::to_string(&env).unwrap().contains("details"));
    }

    #[test]
    fn equal_envelopes_serialize_to_equal_bytes() {
        let a = ErrorEnvelope::busy_retry();
        let b = ErrorEnvelope::busy_retry();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn memory_write_error_code_strings_match_t15_05() {
        assert_eq!(ErrorCode::UnknownMemory.as_str(), "UNKNOWN_MEMORY");
        assert_eq!(
            ErrorCode::OptimisticConflict.as_str(),
            "OPTIMISTIC_CONFLICT"
        );
        assert_eq!(
            ErrorCode::CanonicalKeyConflict.as_str(),
            "CANONICAL_KEY_CONFLICT"
        );
        assert_eq!(
            ErrorCode::InvalidGlobalScopeOwner.as_str(),
            "INVALID_GLOBAL_SCOPE_OWNER"
        );
        assert_eq!(
            ErrorCode::IllegalMemoryTransition.as_str(),
            "ILLEGAL_MEMORY_TRANSITION"
        );
        assert_eq!(ErrorCode::EntryTerminal.as_str(), "ENTRY_TERMINAL");
        assert_eq!(ErrorCode::IncompatibleScope.as_str(), "INCOMPATIBLE_SCOPE");
        assert_eq!(ErrorCode::EmptyMergeSet.as_str(), "EMPTY_MERGE_SET");
        assert_eq!(
            ErrorCode::ModelClaimOnlyProvenance.as_str(),
            "MODEL_CLAIM_ONLY_PROVENANCE"
        );
        assert_eq!(ErrorCode::UnknownCandidate.as_str(), "UNKNOWN_CANDIDATE");
        assert_eq!(
            ErrorCode::IllegalCandidateTransition.as_str(),
            "ILLEGAL_CANDIDATE_TRANSITION"
        );
        assert_eq!(
            ErrorCode::CandidateNotPending.as_str(),
            "CANDIDATE_NOT_PENDING"
        );
        assert_eq!(
            ErrorCode::InvalidProposedOperation.as_str(),
            "INVALID_PROPOSED_OPERATION"
        );
    }

    #[test]
    fn memory_write_error_constructors_are_never_retryable() {
        assert!(!ErrorEnvelope::unknown_memory().retryable);
        assert!(!ErrorEnvelope::optimistic_conflict(1, 2).retryable);
        assert!(!ErrorEnvelope::canonical_key_conflict().retryable);
        assert!(!ErrorEnvelope::invalid_global_scope_owner().retryable);
        assert!(!ErrorEnvelope::illegal_memory_transition("x").retryable);
        assert!(!ErrorEnvelope::entry_terminal().retryable);
        assert!(!ErrorEnvelope::incompatible_scope().retryable);
        assert!(!ErrorEnvelope::empty_merge_set().retryable);
        assert!(!ErrorEnvelope::model_claim_only_provenance().retryable);
        assert!(!ErrorEnvelope::unknown_candidate().retryable);
        assert!(!ErrorEnvelope::illegal_candidate_transition("x").retryable);
        assert!(!ErrorEnvelope::candidate_not_pending().retryable);
        assert!(!ErrorEnvelope::invalid_proposed_operation("x").retryable);
    }

    #[test]
    fn optimistic_conflict_names_both_versions_in_details() {
        let env = ErrorEnvelope::optimistic_conflict(3, 5);
        assert_eq!(env.code, ErrorCode::OptimisticConflict);
        let details = env.details.expect("details");
        assert!(details.contains('3'), "{details}");
        assert!(details.contains('5'), "{details}");
    }

    #[test]
    fn illegal_memory_transition_carries_the_caller_supplied_details() {
        let env = ErrorEnvelope::illegal_memory_transition("(hypothesis) active -> retracted");
        assert_eq!(env.code, ErrorCode::IllegalMemoryTransition);
        assert_eq!(
            env.details.as_deref(),
            Some("(hypothesis) active -> retracted")
        );
    }

    #[test]
    fn invalid_proposed_operation_carries_the_parse_error_string() {
        let env = ErrorEnvelope::invalid_proposed_operation("missing field `kind`");
        assert_eq!(env.code, ErrorCode::InvalidProposedOperation);
        assert_eq!(env.details.as_deref(), Some("missing field `kind`"));
    }

    #[test]
    fn no_details_variants_omit_details_from_the_wire() {
        for env in [
            ErrorEnvelope::unknown_memory(),
            ErrorEnvelope::canonical_key_conflict(),
            ErrorEnvelope::invalid_global_scope_owner(),
            ErrorEnvelope::entry_terminal(),
            ErrorEnvelope::incompatible_scope(),
            ErrorEnvelope::empty_merge_set(),
            ErrorEnvelope::model_claim_only_provenance(),
            ErrorEnvelope::unknown_candidate(),
            ErrorEnvelope::candidate_not_pending(),
        ] {
            assert_eq!(env.details, None);
            assert!(!serde_json::to_string(&env).unwrap().contains("details"));
        }
    }
}
