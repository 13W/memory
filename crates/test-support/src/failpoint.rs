//! Named, `fail_point!`-style injection points.
//!
//! The projection fault matrix (F1–F12, spec 05 §10) and the spool kill matrix
//! (S1–S8, spec 07 §7) need deterministic crash points: "kill the process
//! *here*", "make this op return an error *here*". This module provides the
//! mechanism; the individual matrix rows are authored later (T07-05, T13-06).
//!
//! # Registry-strict semantics
//!
//! A failpoint name must be *declared* before it can be armed. Arming an
//! undeclared name is rejected with [`FailpointError::Unknown`] — this catches
//! typos in test code that would otherwise silently arm a point that never
//! fires. Injection sites declare their own name when hit (see [`fail_point!`](crate::fail_point)),
//! and tests may declare names up front via [`Failpoints::register`].
//!
//! # Adoption pattern (later tasks)
//!
//! Product injection sites call [`fail_point!`](crate::fail_point), which consults the process
//! [`global`] registry. To keep release builds zero-cost, product crates will
//! take this harness as an *optional* dependency behind a `failpoints` cargo
//! feature and gate the macro calls on it. No product site exists yet at
//! T00-03; the mechanism is delivered and self-tested here.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock};

/// What an armed failpoint does when its site is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Panic, unwinding the current thread.
    Panic,
    /// Abort the whole process immediately — the crash/kill simulation used by
    /// the F/S matrices to model power loss between two operations.
    Abort,
    /// Signal that the site should take its error branch. Meaningful only with
    /// the two-argument [`fail_point!`](crate::fail_point) form, which returns the caller's error
    /// value; [`Action::execute`] treats it as a no-op.
    Error,
}

impl Action {
    /// Perform the immediate effect of this action at site `name`.
    ///
    /// [`Action::Panic`] and [`Action::Abort`] diverge; [`Action::Error`] is a
    /// no-op because it is realized by the caller returning its own error value.
    pub fn execute(self, name: &str) {
        match self {
            Action::Panic => panic!("failpoint `{name}` fired: panic"),
            Action::Abort => std::process::abort(),
            Action::Error => {}
        }
    }
}

/// Error type for failpoint registry operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailpointError {
    /// An operation referenced a failpoint name that was never declared.
    Unknown(String),
}

impl fmt::Display for FailpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailpointError::Unknown(name) => {
                write!(f, "unknown failpoint `{name}`: declare it before arming")
            }
        }
    }
}

impl std::error::Error for FailpointError {}

/// A registry of named failpoints.
///
/// Each declared name maps to an optional armed [`Action`]. The registry is
/// internally synchronized, so it can be shared across threads.
///
/// ```
/// use local_rag_test_support::{Action, Failpoints};
/// let fp = Failpoints::new();
/// fp.register("proj.write_ahead");
/// assert!(fp.arm("proj.write_ahead", Action::Abort).is_ok());
/// assert_eq!(fp.eval("proj.write_ahead").unwrap(), Some(Action::Abort));
/// // Arming an undeclared name is rejected.
/// assert!(fp.arm("typo.here", Action::Abort).is_err());
/// ```
#[derive(Debug, Default)]
pub struct Failpoints {
    points: Mutex<HashMap<String, Option<Action>>>,
}

impl Failpoints {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a failpoint name, idempotently. Existing arming is preserved.
    pub fn register(&self, name: &str) {
        self.lock().entry(name.to_owned()).or_insert(None);
    }

    /// Arm a declared failpoint with `action`.
    ///
    /// Returns [`FailpointError::Unknown`] if `name` was never declared.
    pub fn arm(&self, name: &str, action: Action) -> Result<(), FailpointError> {
        match self.lock().get_mut(name) {
            Some(slot) => {
                *slot = Some(action);
                Ok(())
            }
            None => Err(FailpointError::Unknown(name.to_owned())),
        }
    }

    /// Disarm a declared failpoint, leaving it registered.
    ///
    /// Returns [`FailpointError::Unknown`] if `name` was never declared.
    pub fn disarm(&self, name: &str) -> Result<(), FailpointError> {
        match self.lock().get_mut(name) {
            Some(slot) => {
                *slot = None;
                Ok(())
            }
            None => Err(FailpointError::Unknown(name.to_owned())),
        }
    }

    /// Return the action armed on `name`, if any.
    ///
    /// Returns [`FailpointError::Unknown`] if `name` was never declared, `Ok(None)`
    /// if declared but not armed, and `Ok(Some(action))` if armed. This is the
    /// accessor an injection site uses.
    pub fn eval(&self, name: &str) -> Result<Option<Action>, FailpointError> {
        match self.lock().get(name) {
            Some(slot) => Ok(*slot),
            None => Err(FailpointError::Unknown(name.to_owned())),
        }
    }

    /// Whether `name` has been declared.
    pub fn is_declared(&self, name: &str) -> bool {
        self.lock().contains_key(name)
    }

    /// Remove all declarations and armings. Use in test teardown.
    pub fn reset(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<Action>>> {
        self.points
            .lock()
            .expect("failpoint registry mutex poisoned")
    }
}

/// The process-global failpoint registry consulted by [`fail_point!`](crate::fail_point).
pub fn global() -> &'static Failpoints {
    static GLOBAL: OnceLock<Failpoints> = OnceLock::new();
    GLOBAL.get_or_init(Failpoints::new)
}

/// Declare and evaluate a named injection point against the [`global`] registry.
///
/// The single-argument form performs the armed action in place (panic/abort for
/// [`Action::Panic`]/[`Action::Abort`]; nothing otherwise). The two-argument
/// form additionally returns `$ret` from the enclosing function when the point
/// is armed — use it to inject an error return:
///
/// ```
/// use local_rag_test_support::{fail_point, Action};
/// use local_rag_test_support::failpoint::global;
///
/// fn commit() -> Result<(), &'static str> {
///     fail_point!("demo.commit", Err("injected"));
///     Ok(())
/// }
///
/// global().register("demo.commit");
/// assert_eq!(commit(), Ok(()));
/// global().arm("demo.commit", Action::Error).unwrap();
/// assert_eq!(commit(), Err("injected"));
/// ```
#[macro_export]
macro_rules! fail_point {
    ($name:expr $(,)?) => {{
        let __fp = $crate::failpoint::global();
        __fp.register($name);
        if let Ok(Some(__action)) = __fp.eval($name) {
            __action.execute($name);
        }
    }};
    ($name:expr, $ret:expr $(,)?) => {{
        let __fp = $crate::failpoint::global();
        __fp.register($name);
        if let Ok(Some(__action)) = __fp.eval($name) {
            __action.execute($name);
            return $ret;
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_disarm_eval_roundtrip() {
        let fp = Failpoints::new();
        fp.register("a");
        assert_eq!(fp.eval("a").unwrap(), None);
        fp.arm("a", Action::Panic).unwrap();
        assert_eq!(fp.eval("a").unwrap(), Some(Action::Panic));
        fp.disarm("a").unwrap();
        assert_eq!(fp.eval("a").unwrap(), None);
    }

    #[test]
    fn undeclared_operations_are_rejected() {
        let fp = Failpoints::new();
        assert_eq!(
            fp.arm("nope", Action::Abort),
            Err(FailpointError::Unknown("nope".to_owned()))
        );
        assert!(matches!(fp.eval("nope"), Err(FailpointError::Unknown(_))));
        assert!(matches!(fp.disarm("nope"), Err(FailpointError::Unknown(_))));
    }

    #[test]
    fn register_is_idempotent_and_preserves_arming() {
        let fp = Failpoints::new();
        fp.register("a");
        fp.arm("a", Action::Abort).unwrap();
        fp.register("a"); // must not clobber the armed action
        assert_eq!(fp.eval("a").unwrap(), Some(Action::Abort));
    }

    #[test]
    fn macro_error_form_returns_on_arm() {
        fn op() -> Result<u32, &'static str> {
            fail_point!("ts.macro.err", Err("boom"));
            Ok(7)
        }
        // Unique name; not reset to avoid clobbering any concurrent global user.
        assert_eq!(op(), Ok(7));
        global().arm("ts.macro.err", Action::Error).unwrap();
        assert_eq!(op(), Err("boom"));
        global().disarm("ts.macro.err").unwrap();
        assert_eq!(op(), Ok(7));
    }
}
