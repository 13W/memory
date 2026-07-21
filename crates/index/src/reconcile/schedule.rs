//! The reconcile scheduling engine (spec 06 §1 "Triggers & scheduling") — T05-04,
//! Layer 1.
//!
//! [`Debouncer`] is the **pure** heart of the scheduler: it coalesces triggers,
//! escalates the scan mode, resets the debounce quiet window, and self-injects the
//! periodic reconcile — all as a function of an **explicit** monotonic `now_ms: i64`.
//! It performs no I/O and touches no clock, mirroring the codebase's other
//! time-as-a-parameter seams ([`build_generation`](super::build_generation) and
//! [`uuidv7_from`](local_rag_core::identity::uuidv7_from)). The async driver
//! ([`super::driver`]) turns [`next_wake`](Debouncer::next_wake) into a real timer;
//! every fine-grained timing rule is decided here so it is deterministically
//! unit-testable over plain integers.
//!
//! **Principle (spec 06, `[FIXED]`): watcher = hint, reconcile = truth.** Triggers
//! only *schedule* work; correctness comes from the authoritative scan+build the
//! driver runs when a deadline is reached.

use crate::scan::ScanMode;

/// The debounce quiet window — `[SPEC: 500 ms quiet window]` (spec 06 §1).
///
/// A stream of filesystem events collapses into one reconcile once the tree has
/// been quiet for this long. Lives here as an index-crate constant, not in
/// `core::config::Config`, so the frozen spec 02 §3.1 config surface (and its pinned
/// `default_matches_spec_toml`) is unaffected.
pub const DEBOUNCE_MS: i64 = 500;

/// The periodic strict-reconcile interval — `[SPEC: every 6 h while open]`
/// (spec 06 §1): a backstop that re-establishes truth even if every event was lost.
pub const PERIODIC_MS: i64 = 6 * 60 * 60 * 1000;

/// The two tunable intervals of the scheduling engine (both `[SPEC]`, spec 06 §1).
///
/// [`Default`] is the spec pair; tests may shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleConfig {
    /// The debounce quiet window in milliseconds.
    pub debounce_ms: i64,
    /// The periodic strict-reconcile interval in milliseconds.
    pub periodic_ms: i64,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            debounce_ms: DEBOUNCE_MS,
            periodic_ms: PERIODIC_MS,
        }
    }
}

/// What scheduled a reconcile (spec 06 §1). Each kind maps to a [`ScanMode`] and is
/// either debounced (collapsed under the quiet window) or a bypass that fires
/// immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TriggerKind {
    /// A known worktree was opened — cold start (strict, immediate).
    Startup,
    /// The 6 h periodic backstop (strict, immediate; self-injected by the engine).
    Periodic,
    /// `local-rag reindex` — the manual force (strict, immediate, bypasses debounce).
    Manual,
    /// A filesystem path event from the watcher (fast, debounced).
    FsChange,
    /// A `.git/HEAD` / index change: checkout, rebase, commit (fast, debounced;
    /// dropped for non-git worktrees, spec 06 §6).
    GitHead,
    /// Watcher overflow / forced rescan: mandatory **strict** reconcile, immediate
    /// (spec 06 §1 `[FIXED]`: "never resync from events").
    WatcherOverflow,
}

impl TriggerKind {
    /// The scan mode this trigger requires. `FsChange`/`GitHead` trust the advisory
    /// stat cache ([`ScanMode::Fast`]); every other kind forces a full re-hash
    /// ([`ScanMode::Strict`]).
    pub fn scan_mode(self) -> ScanMode {
        match self {
            TriggerKind::FsChange | TriggerKind::GitHead => ScanMode::Fast,
            TriggerKind::Startup
            | TriggerKind::Periodic
            | TriggerKind::Manual
            | TriggerKind::WatcherOverflow => ScanMode::Strict,
        }
    }

    /// Whether this trigger is subject to the debounce quiet window (only watcher
    /// path/git events are; the rest bypass it and fire immediately).
    fn is_debounced(self) -> bool {
        matches!(self, TriggerKind::FsChange | TriggerKind::GitHead)
    }

    /// Whether this trigger is meaningful only for git worktrees (spec 06 §6).
    fn is_git_only(self) -> bool {
        matches!(self, TriggerKind::GitHead)
    }
}

/// The reconcile the engine decided to run when a deadline was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedReconcile {
    /// The (escalated) scan mode: `Strict` if any coalesced trigger required it.
    pub mode: ScanMode,
    /// `true` iff a bypass trigger (startup/periodic/manual/overflow) drove this
    /// reconcile — i.e. it did not wait out the debounce quiet window.
    pub immediate: bool,
}

/// The single coalesced request the engine is holding.
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// The monotonic millisecond at or after which the reconcile should run.
    due_at: i64,
    /// The escalated scan mode (`Strict` wins).
    mode: ScanMode,
    /// `true` while only debounced triggers have contributed; a bypass trigger
    /// clears it and pulls `due_at` to "now".
    debounced: bool,
}

/// `Strict` if either mode is `Strict`, else `Fast` (`ScanMode` has no `Ord`).
fn stricter(a: ScanMode, b: ScanMode) -> ScanMode {
    if matches!(a, ScanMode::Strict) || matches!(b, ScanMode::Strict) {
        ScanMode::Strict
    } else {
        ScanMode::Fast
    }
}

/// The per-worktree scheduling state machine (spec 06 §1). Pure: driven entirely by
/// [`record`](Self::record)/[`take_due`](Self::take_due) with an explicit `now_ms`.
#[derive(Debug, Clone)]
pub struct Debouncer {
    cfg: ScheduleConfig,
    is_git: bool,
    pending: Option<Pending>,
    next_periodic_at: i64,
}

impl Debouncer {
    /// A fresh scheduler for a worktree of the given git-ness, with the first
    /// periodic backstop scheduled one interval after `now_ms`.
    pub fn new(cfg: ScheduleConfig, is_git: bool, now_ms: i64) -> Self {
        Self {
            cfg,
            is_git,
            pending: None,
            next_periodic_at: now_ms.saturating_add(cfg.periodic_ms),
        }
    }

    /// Fold a trigger into the pending request (spec 06 §1). Coalesces into a single
    /// reconcile, escalates the mode (`Strict` wins), and either resets the debounce
    /// quiet window (debounced kinds) or pulls the request to "now" (bypass kinds).
    /// `GitHead` on a non-git worktree is a no-op (spec 06 §6).
    pub fn record(&mut self, kind: TriggerKind, now_ms: i64) {
        if kind.is_git_only() && !self.is_git {
            return;
        }
        let mode = kind.scan_mode();
        match &mut self.pending {
            None => {
                let (due_at, debounced) = if kind.is_debounced() {
                    (now_ms.saturating_add(self.cfg.debounce_ms), true)
                } else {
                    (now_ms, false)
                };
                self.pending = Some(Pending {
                    due_at,
                    mode,
                    debounced,
                });
            }
            Some(p) => {
                p.mode = stricter(p.mode, mode);
                if kind.is_debounced() {
                    // Reset the quiet window — but never *delay* a request that a
                    // bypass trigger already made immediate.
                    if p.debounced {
                        p.due_at = now_ms.saturating_add(self.cfg.debounce_ms);
                    }
                } else {
                    // Bypass: fire immediately (and never later than an existing
                    // immediate deadline).
                    p.due_at = p.due_at.min(now_ms);
                    p.debounced = false;
                }
            }
        }
    }

    /// The next monotonic millisecond the driver must wake at: the sooner of the
    /// pending request's deadline and the next periodic backstop.
    pub fn next_wake(&self) -> i64 {
        match self.pending {
            Some(p) => p.due_at.min(self.next_periodic_at),
            None => self.next_periodic_at,
        }
    }

    /// If a reconcile is due at `now_ms`, take it and clear the pending request.
    ///
    /// First self-injects a [`Periodic`](TriggerKind::Periodic) trigger when the
    /// backstop interval has elapsed (advancing it past `now_ms`), then fires the
    /// pending request if its deadline has been reached.
    pub fn take_due(&mut self, now_ms: i64) -> Option<PlannedReconcile> {
        if now_ms >= self.next_periodic_at {
            self.record(TriggerKind::Periodic, now_ms);
            // Advance strictly past `now_ms` (whole intervals) so a long gap does
            // not spin.
            while self.next_periodic_at <= now_ms {
                self.next_periodic_at = self.next_periodic_at.saturating_add(self.cfg.periodic_ms);
            }
        }
        match self.pending {
            Some(p) if now_ms >= p.due_at => {
                self.pending = None;
                Some(PlannedReconcile {
                    mode: p.mode,
                    immediate: !p.debounced,
                })
            }
            _ => None,
        }
    }

    /// Take any pending reconcile regardless of its deadline, used to flush a
    /// scheduled reconcile on graceful shutdown (the trigger channel closed).
    pub fn take_pending(&mut self) -> Option<PlannedReconcile> {
        self.pending.take().map(|p| PlannedReconcile {
            mode: p.mode,
            immediate: !p.debounced,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with the real debounce but an out-of-the-way periodic so periodic
    /// self-injection never interferes with debounce/coalescing assertions.
    fn cfg() -> ScheduleConfig {
        ScheduleConfig {
            debounce_ms: DEBOUNCE_MS,
            periodic_ms: 1_000_000_000,
        }
    }

    #[test]
    fn debounce_resets_on_each_fs_event() {
        let mut d = Debouncer::new(cfg(), true, 0);
        d.record(TriggerKind::FsChange, 0);
        assert_eq!(d.next_wake(), 500, "one event → deadline now+500");
        d.record(TriggerKind::FsChange, 300);
        assert_eq!(d.next_wake(), 800, "a second event resets the quiet window");
        assert_eq!(d.take_due(799), None, "not due before the window elapses");
        assert_eq!(
            d.take_due(800),
            Some(PlannedReconcile {
                mode: ScanMode::Fast,
                immediate: false,
            }),
            "due once the tree has been quiet for 500 ms",
        );
    }

    #[test]
    fn coalescing_collapses_burst_into_one() {
        let mut d = Debouncer::new(cfg(), true, 0);
        d.record(TriggerKind::FsChange, 0);
        d.record(TriggerKind::FsChange, 100);
        d.record(TriggerKind::FsChange, 200);
        // Window last reset at 200 → due at 700.
        assert_eq!(d.take_due(699), None);
        assert!(d.take_due(700).is_some(), "the burst fires exactly once");
        assert_eq!(d.take_due(700), None, "and only once — pending is cleared");
    }

    #[test]
    fn manual_bypasses_debounce() {
        let mut d = Debouncer::new(cfg(), true, 0);
        d.record(TriggerKind::Manual, 0);
        assert_eq!(d.next_wake(), 0, "manual force is immediate");
        assert_eq!(
            d.take_due(0),
            Some(PlannedReconcile {
                mode: ScanMode::Strict,
                immediate: true,
            }),
        );
    }

    #[test]
    fn overflow_forces_strict_immediately() {
        let mut d = Debouncer::new(cfg(), true, 0);
        d.record(TriggerKind::FsChange, 0); // pending Fast, due at 500
        d.record(TriggerKind::WatcherOverflow, 100); // escalate + pull to now
        assert_eq!(d.next_wake(), 100);
        assert_eq!(
            d.take_due(100),
            Some(PlannedReconcile {
                mode: ScanMode::Strict,
                immediate: true,
            }),
            "overflow escalates a pending fast request to immediate strict",
        );
    }

    #[test]
    fn fs_event_never_delays_a_pending_manual() {
        let mut d = Debouncer::new(cfg(), true, 0);
        d.record(TriggerKind::Manual, 0); // immediate strict, due at 0
        d.record(TriggerKind::FsChange, 100); // must not push the deadline out
        assert_eq!(d.next_wake(), 0, "the manual deadline is preserved");
        let plan = d.take_due(0).expect("due immediately");
        assert_eq!(plan.mode, ScanMode::Strict);
        assert!(plan.immediate);
    }

    #[test]
    fn startup_periodic_manual_git_coalesce_into_one_strict() {
        let mut d = Debouncer::new(cfg(), true, 0);
        d.record(TriggerKind::GitHead, 0);
        d.record(TriggerKind::Startup, 10);
        d.record(TriggerKind::Manual, 20);
        // All coalesce into a single immediate strict reconcile.
        assert_eq!(
            d.take_due(20),
            Some(PlannedReconcile {
                mode: ScanMode::Strict,
                immediate: true,
            }),
        );
        assert_eq!(d.take_due(20), None, "only one reconcile results");
    }

    #[test]
    fn periodic_self_injects_every_interval() {
        let period = 1_000_000;
        let mut d = Debouncer::new(
            ScheduleConfig {
                debounce_ms: DEBOUNCE_MS,
                periodic_ms: period,
            },
            true,
            0,
        );
        assert_eq!(d.next_wake(), period, "first backstop one interval out");
        assert_eq!(d.take_due(period - 1), None);
        assert_eq!(
            d.take_due(period),
            Some(PlannedReconcile {
                mode: ScanMode::Strict,
                immediate: true,
            }),
            "the 6 h backstop self-injects a strict reconcile",
        );
        assert_eq!(d.next_wake(), 2 * period, "the backstop re-arms");
    }

    #[test]
    fn git_head_is_dropped_for_non_git_worktrees() {
        let mut d = Debouncer::new(cfg(), false, 0);
        d.record(TriggerKind::GitHead, 0);
        assert!(
            d.pending.is_none(),
            "a non-git worktree has no git triggers (spec 06 §6)",
        );
        assert_eq!(d.take_due(1_000), None);
    }

    #[test]
    fn scan_mode_selection_table() {
        assert_eq!(TriggerKind::FsChange.scan_mode(), ScanMode::Fast);
        assert_eq!(TriggerKind::GitHead.scan_mode(), ScanMode::Fast);
        assert_eq!(TriggerKind::Startup.scan_mode(), ScanMode::Strict);
        assert_eq!(TriggerKind::Periodic.scan_mode(), ScanMode::Strict);
        assert_eq!(TriggerKind::Manual.scan_mode(), ScanMode::Strict);
        assert_eq!(TriggerKind::WatcherOverflow.scan_mode(), ScanMode::Strict);
    }
}
