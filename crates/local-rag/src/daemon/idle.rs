//! Idle-shutdown gating (spec 02 §4.3 `[FIXED]`: idle shutdown only when
//! **all** hold: no live MCP sessions, no unimported spool bytes, no running
//! index/consolidation/GC jobs) — T15-01.
//!
//! [`idle_eligible`] is the pure three-input predicate; the timer that polls
//! it and debounces against `idle_shutdown_secs` (spec 02 §3.1) is wired
//! into `daemon::lifecycle` (T15-01), which is where the config value, the
//! live [`super::session::SessionRegistry`]/[`super::jobs::JobRegistry`],
//! and the shutdown trigger all actually come together.

/// The three facts spec 02 §4.3's idle-shutdown gate reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleGateInputs {
    /// Live MCP session count ([`super::session::SessionRegistry::len`]).
    pub live_sessions: usize,
    /// Whether the store has spool bytes not yet imported
    /// ([`local_rag_store::store_has_pending_spool_bytes`]).
    pub pending_spool_bytes: bool,
    /// Running background job count ([`super::jobs::JobRegistry::len`]).
    pub running_jobs: usize,
}

/// Whether the daemon is currently eligible for idle shutdown.
///
/// Pure and total: all three conditions must hold simultaneously (spec 02
/// §4.3's "**all**"), so a single non-idle input refuses regardless of the
/// other two.
pub fn idle_eligible(inputs: &IdleGateInputs) -> bool {
    inputs.live_sessions == 0 && !inputs.pending_spool_bytes && inputs.running_jobs == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_idle() -> IdleGateInputs {
        IdleGateInputs {
            live_sessions: 0,
            pending_spool_bytes: false,
            running_jobs: 0,
        }
    }

    #[test]
    fn all_three_conditions_idle_is_eligible() {
        assert!(idle_eligible(&all_idle()));
    }

    #[test]
    fn a_live_session_alone_refuses() {
        let inputs = IdleGateInputs {
            live_sessions: 1,
            ..all_idle()
        };
        assert!(!idle_eligible(&inputs));
    }

    #[test]
    fn pending_spool_bytes_alone_refuses() {
        let inputs = IdleGateInputs {
            pending_spool_bytes: true,
            ..all_idle()
        };
        assert!(!idle_eligible(&inputs));
    }

    #[test]
    fn a_running_job_alone_refuses() {
        let inputs = IdleGateInputs {
            running_jobs: 1,
            ..all_idle()
        };
        assert!(!idle_eligible(&inputs));
    }

    #[test]
    fn every_condition_failing_at_once_still_refuses() {
        let inputs = IdleGateInputs {
            live_sessions: 3,
            pending_spool_bytes: true,
            running_jobs: 2,
        };
        assert!(!idle_eligible(&inputs));
    }
}
