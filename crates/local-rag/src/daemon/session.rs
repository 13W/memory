//! Live MCP session tracking (spec 02 §4.3 idle-shutdown gate: "no live MCP
//! sessions") — T15-01.
//!
//! Spec 02 §3.3's own as-built note is explicit that `session_id` is
//! "routing/telemetry only... not part of identity resolution", and the
//! section describes no session-registry mechanism of its own. `session_id`
//! only becomes real at the HELLO handshake (spec 02 §4.2, T15-02, not yet
//! built), so this registry is deliberately protocol-agnostic: T15-01's own
//! tests call [`SessionRegistry::register`] directly (no real MCP handshake
//! needed to prove the idle-shutdown gate reacts correctly), and T15-02
//! later calls it from its real per-connection HELLO handler, holding the
//! returned guard for the connection's lifetime — purely additive wiring, no
//! API change needed here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct Inner {
    sessions: Mutex<HashMap<u64, String>>,
    next_token: AtomicU64,
}

/// A registry of live session ids.
///
/// Cheaply cloneable (shares the same underlying map); every clone observes
/// the same set of registrations.
#[derive(Debug, Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<Inner>,
}

impl SessionRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `session_id` as live. The session is deregistered when the
    /// returned [`SessionGuard`] drops.
    ///
    /// Each registration gets its own internal token — two connections that
    /// happen to carry the same `session_id` string (or a reconnect using
    /// the same id) never collide; each has its own guard and its own
    /// deregistration.
    pub fn register(&self, session_id: impl Into<String>) -> SessionGuard {
        let token = self.inner.next_token.fetch_add(1, Ordering::Relaxed);
        self.inner
            .sessions
            .lock()
            .expect("session registry mutex poisoned")
            .insert(token, session_id.into());
        SessionGuard {
            inner: Arc::clone(&self.inner),
            token,
        }
    }

    /// The number of currently live sessions.
    pub fn len(&self) -> usize {
        self.inner
            .sessions
            .lock()
            .expect("session registry mutex poisoned")
            .len()
    }

    /// Whether there are no live sessions.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// An RAII handle for one registered session: deregisters on drop.
#[derive(Debug)]
pub struct SessionGuard {
    inner: Arc<Inner>,
    token: u64,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.inner
            .sessions
            .lock()
            .expect("session registry mutex poisoned")
            .remove(&self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_registry_is_empty() {
        let registry = SessionRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn registering_grows_and_dropping_shrinks() {
        let registry = SessionRegistry::new();
        let a = registry.register("session-a");
        assert_eq!(registry.len(), 1);
        let b = registry.register("session-b");
        assert_eq!(registry.len(), 2);

        drop(a);
        assert_eq!(registry.len(), 1);
        drop(b);
        assert!(registry.is_empty());
    }

    #[test]
    fn duplicate_session_id_strings_do_not_collide() {
        let registry = SessionRegistry::new();
        let a = registry.register("same-id");
        let b = registry.register("same-id");
        assert_eq!(registry.len(), 2);
        drop(a);
        assert_eq!(registry.len(), 1, "dropping one must not remove both");
        drop(b);
        assert!(registry.is_empty());
    }

    #[test]
    fn clones_observe_the_same_registrations() {
        let registry = SessionRegistry::new();
        let clone = registry.clone();
        let _guard = registry.register("session-a");
        assert_eq!(clone.len(), 1);
    }
}
