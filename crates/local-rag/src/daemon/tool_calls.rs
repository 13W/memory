//! In-memory `tools/call` counters (spec 11 §2, T19-05, group 19 plan) —
//! per-session and since-daemon-start aggregate, feeding `stats`'s own
//! `tool_calls` field. Deliberately no persistence (the group 19 plan's own
//! scope boundary, distinct from an audit trail): a daemon restart resets
//! both. This is what turns D-041's own stated limitation — "agentic
//! behavioral compliance is not unit-testable" — into an observable number
//! instead of an impression.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct SessionEntry {
    counts: HashMap<String, u64>,
    open_connections: u32,
}

#[derive(Debug, Default)]
struct Inner {
    aggregate: Mutex<HashMap<String, u64>>,
    sessions: Mutex<HashMap<String, SessionEntry>>,
}

/// Counts `tools/call` invocations by tool name, both per session
/// (`session_id` — one Claude Code `local-rag-proxy` connection, spec 02
/// §3.3) and as a running total since the daemon started. Cheaply
/// cloneable, like [`super::session::SessionRegistry`] — every clone shares
/// the same underlying counters.
///
/// Per-session counts are ref-counted by open connection, not by a
/// per-connection token the way [`super::session::SessionRegistry`] is:
/// two connections that happen to share the same `session_id` string (a
/// rare case — `session_id` is normally a fresh UUID per `local-rag-proxy`
/// process) accumulate into the same bucket, and the bucket is only
/// cleared once every connection for that id has closed, so one
/// connection's disconnect can never erase another still-live connection's
/// counts. Unlike `SessionRegistry`, where a false "no live sessions"
/// reading is a real correctness bug (the idle-shutdown gate), a shared
/// bucket here is a deliberate, harmless simplification — this is an
/// observability metric, not a liveness gate.
#[derive(Debug, Clone, Default)]
pub struct ToolCallCounters {
    inner: Arc<Inner>,
}

impl ToolCallCounters {
    /// A fresh set of counters, no calls recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin tracking `session_id`. Its per-session bucket is cleared once
    /// every [`ToolCallSessionGuard`] for that id has dropped; the
    /// aggregate is never cleared.
    pub fn begin_session(&self, session_id: impl Into<String>) -> ToolCallSessionGuard {
        let session_id = session_id.into();
        self.inner
            .sessions
            .lock()
            .expect("tool-call counters mutex poisoned")
            .entry(session_id.clone())
            .or_default()
            .open_connections += 1;
        ToolCallSessionGuard {
            inner: Arc::clone(&self.inner),
            session_id,
        }
    }

    /// Record one `tools/call` invocation of `tool_name` for `session_id`.
    /// Counted whether or not the call ultimately succeeds — an attempted
    /// call, not a successful one: a real MCP client only ever sends names
    /// from the catalog it fetched, so an unrecognized name here is either
    /// a malformed/adversarial client (a harmless one-off entry, not worth
    /// a dedicated validation path) or a genuine protocol bug worth seeing
    /// in the counts, not silently dropping.
    pub fn record(&self, session_id: &str, tool_name: &str) {
        *self
            .inner
            .aggregate
            .lock()
            .expect("tool-call counters mutex poisoned")
            .entry(tool_name.to_string())
            .or_insert(0) += 1;
        *self
            .inner
            .sessions
            .lock()
            .expect("tool-call counters mutex poisoned")
            .entry(session_id.to_string())
            .or_default()
            .counts
            .entry(tool_name.to_string())
            .or_insert(0) += 1;
    }

    /// `session_id`'s own counts, sorted by tool name — deterministic
    /// output for `stats`'s wire JSON and fixture tests.
    pub fn session_snapshot(&self, session_id: &str) -> Vec<(String, u64)> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .expect("tool-call counters mutex poisoned");
        let mut rows: Vec<(String, u64)> = sessions
            .get(session_id)
            .map(|entry| entry.counts.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// Every tool call counted since the daemon started, sorted by tool
    /// name.
    pub fn aggregate_snapshot(&self) -> Vec<(String, u64)> {
        let aggregate = self
            .inner
            .aggregate
            .lock()
            .expect("tool-call counters mutex poisoned");
        let mut rows: Vec<(String, u64)> = aggregate.iter().map(|(k, v)| (k.clone(), *v)).collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }
}

/// An RAII handle for one connection's participation in a session's
/// tool-call counts. On drop, decrements that session's open-connection
/// count; the bucket is removed only once the count reaches zero.
#[derive(Debug)]
pub struct ToolCallSessionGuard {
    inner: Arc<Inner>,
    session_id: String,
}

impl Drop for ToolCallSessionGuard {
    fn drop(&mut self) {
        let mut sessions = self
            .inner
            .sessions
            .lock()
            .expect("tool-call counters mutex poisoned");
        if let Some(entry) = sessions.get_mut(&self.session_id) {
            entry.open_connections = entry.open_connections.saturating_sub(1);
            if entry.open_connections == 0 {
                sessions.remove(&self.session_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_registry_has_no_counts() {
        let counters = ToolCallCounters::new();
        assert_eq!(counters.aggregate_snapshot(), vec![]);
        assert_eq!(counters.session_snapshot("sess-1"), vec![]);
    }

    #[test]
    fn recording_increments_both_session_and_aggregate() {
        let counters = ToolCallCounters::new();
        let _guard = counters.begin_session("sess-1");
        counters.record("sess-1", "recall");
        counters.record("sess-1", "recall");
        counters.record("sess-1", "search_code");
        assert_eq!(
            counters.session_snapshot("sess-1"),
            vec![("recall".to_string(), 2), ("search_code".to_string(), 1)]
        );
        assert_eq!(
            counters.aggregate_snapshot(),
            vec![("recall".to_string(), 2), ("search_code".to_string(), 1)]
        );
    }

    #[test]
    fn two_sessions_stay_isolated_but_aggregate_sums_both() {
        let counters = ToolCallCounters::new();
        let _a = counters.begin_session("sess-a");
        let _b = counters.begin_session("sess-b");
        counters.record("sess-a", "recall");
        counters.record("sess-b", "recall");
        counters.record("sess-b", "recall");
        assert_eq!(
            counters.session_snapshot("sess-a"),
            vec![("recall".to_string(), 1)]
        );
        assert_eq!(
            counters.session_snapshot("sess-b"),
            vec![("recall".to_string(), 2)]
        );
        assert_eq!(
            counters.aggregate_snapshot(),
            vec![("recall".to_string(), 3)]
        );
    }

    #[test]
    fn dropping_the_guard_clears_the_session_bucket_but_not_the_aggregate() {
        let counters = ToolCallCounters::new();
        let guard = counters.begin_session("sess-1");
        counters.record("sess-1", "recall");
        drop(guard);
        assert_eq!(counters.session_snapshot("sess-1"), vec![]);
        assert_eq!(
            counters.aggregate_snapshot(),
            vec![("recall".to_string(), 1)]
        );
    }

    #[test]
    fn two_guards_sharing_a_session_id_only_clear_once_both_drop() {
        let counters = ToolCallCounters::new();
        let a = counters.begin_session("shared");
        let b = counters.begin_session("shared");
        counters.record("shared", "recall");
        drop(a);
        assert_eq!(
            counters.session_snapshot("shared"),
            vec![("recall".to_string(), 1)],
            "one of two guards dropping must not erase a still-live connection's counts"
        );
        drop(b);
        assert_eq!(counters.session_snapshot("shared"), vec![]);
    }

    #[test]
    fn recording_for_a_session_with_no_guard_yet_is_still_counted() {
        // Defensive characterization, not a supported flow: dispatch.rs
        // always holds a guard (registered in handshake.rs) before any
        // record() can fire for that connection. A stray record() with no
        // matching guard still counts correctly rather than panicking or
        // losing data — it just has no guard to eventually clear it,
        // which cannot happen through the real wiring.
        let counters = ToolCallCounters::new();
        counters.record("orphan", "recall");
        assert_eq!(
            counters.session_snapshot("orphan"),
            vec![("recall".to_string(), 1)]
        );
    }

    #[test]
    fn clones_observe_the_same_counts() {
        let counters = ToolCallCounters::new();
        let clone = counters.clone();
        let _guard = counters.begin_session("sess-1");
        counters.record("sess-1", "recall");
        assert_eq!(
            clone.session_snapshot("sess-1"),
            vec![("recall".to_string(), 1)]
        );
    }
}
