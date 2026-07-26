//! The canonical daemon-protocol error/degraded vocabulary (spec 02 §6) — T09-03.
//!
//! Shared by every daemon subsystem (code search, memory, ...), not just code
//! search — hence living here rather than in `local-rag-search`. Variants are
//! added by the task that first *detects* the condition, never ahead of it
//! (that would be unused, undead code): T09-03's search skeleton defined the
//! first three, and T11-03's embedding provider pool added
//! `POLICY_BLOCKED_REMOTE` — it is the central remote-policy guard (spec 12 §1,
//! 10 §1) and therefore the first code that can refuse a call on policy
//! grounds. Spec 02 §6's remaining rows (`MIGRATION_IN_PROGRESS`,
//! `STORE_LOCKED`, `INCOMPATIBLE_STORE`) still belong to their own later tasks.
//!
//! [`ErrorEnvelope`] is the wire shape `{code, message, retryable, details?}`;
//! `details` stays a freeform `Option<String>` (spec marks its shape
//! unfixed) — no JSON (de)serialization is wired here (`local-rag-protocol`
//! has no `serde` dependency yet), that is group 15's concern.

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
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The canonical daemon protocol error envelope (spec 02 §6:
/// `{code, message, retryable: bool, details?}`). MCP tools map `code` into
/// `isError` content with the same code string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEnvelope {
    /// The canonical error code.
    pub code: ErrorCode,
    /// A human-readable message.
    pub message: String,
    /// Whether the caller should retry (only [`ErrorCode::BusyRetry`] today).
    pub retryable: bool,
    /// Freeform diagnostic detail (spec 02 §6: "every degraded search response
    /// includes the validation reason").
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
    fn only_busy_retry_is_retryable() {
        assert!(!ErrorEnvelope::index_unavailable("both legs down").retryable);
        assert!(!ErrorEnvelope::worktree_not_indexed().retryable);
        assert!(ErrorEnvelope::busy_retry().retryable);
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
}
