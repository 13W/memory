//! Debug/test-only enforcement of spec 02 §5's strict lock-acquisition order.
//!
//! The ambient "what level does this task currently hold" state is
//! [`tokio::task_local!`], not a plain `thread_local!`: `L2.read` is documented
//! to span an entire async pipeline (spec 06 §3), held across multiple
//! `.await` points, and a task can resume on a different OS thread after any of
//! them under a work-stealing runtime — a `thread_local!` stack would silently
//! stop tracking the right task on that hop. `tokio::task::LocalKey`'s storage
//! lives inside the polled future itself and is only swapped into a
//! thread-local for the duration of one poll, which is what makes it safe
//! across that migration.
//!
//! `task_local!`'s only mutators are `scope`/`sync_scope` — there is no "set
//! now, clear later via `Drop`" API. That is why every acquisition below is a
//! **scoped closure/future** ("run this critical section for me"), never a
//! bare RAII guard returned to the caller: by the time an `async fn` could
//! return such a guard, its own `.scope()` call would have already unwound and
//! taken the recorded level with it. This matches the codebase's existing
//! `StateWriter::transaction<F, R>(&self, f: F) -> Result<R, WriteError>` shape.

use std::fmt;
use std::future::Future;

use super::level::LockLevel;

tokio::task_local! {
    static CURRENT_LEVEL: LockLevel;
}

/// A lock was acquired out of the strict order required by spec 02 §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderViolation {
    /// The level already held by this task/thread.
    pub held: LockLevel,
    /// The level whose acquisition was attempted.
    pub attempted: LockLevel,
}

impl fmt::Display for OrderViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "attempted to acquire {:?} (rank {}) while holding {:?} (rank {}); \
             spec 02 §5 requires strictly increasing rank, no exceptions",
            self.attempted,
            self.attempted.rank(),
            self.held,
            self.held.rank(),
        )
    }
}

impl std::error::Error for OrderViolation {}

/// Pure order check: `attempted` is legal only if `held` is `None` or its rank
/// is strictly less than `attempted`'s. No ambient state, no I/O — the single
/// source of truth the scoped helpers below are built on.
pub fn check_order(held: Option<LockLevel>, attempted: LockLevel) -> Result<(), OrderViolation> {
    match held {
        Some(held) if attempted.rank() <= held.rank() => Err(OrderViolation { held, attempted }),
        _ => Ok(()),
    }
}

/// The level this task (or, outside any task-local scope, this OS thread)
/// currently holds, if any.
pub fn held_level() -> Option<LockLevel> {
    CURRENT_LEVEL.try_with(|&l| l).ok()
}

/// `debug_assert!`-gated (compiled out of `--release`, mirroring this crate's
/// existing cost-boundary `debug_assert!` sites — e.g. `code::source::
/// prepare_source`, `ByteSpan::new`): panics with an [`OrderViolation`] if
/// acquiring `attempted` would violate spec 02 §5's order, checked *before*
/// the real acquisition/scope begins. Both `check_order` calls below are
/// macro arguments, so — like the condition and message of any `debug_assert!`
/// — neither runs when `debug_assertions` is off.
fn assert_order(attempted: LockLevel) {
    debug_assert!(
        check_order(held_level(), attempted).is_ok(),
        "{}",
        check_order(held_level(), attempted).expect_err("condition just proved this is Err")
    );
}

/// Run a synchronous, non-`.await`ing body with `level` recorded as this
/// task's (or plain OS thread's — this works with no Tokio runtime present,
/// see the module docs) held level for the body's duration.
///
/// Used for levels whose critical section never awaits: `L1` (`MigrationLock`
/// is acquired and used synchronously) and each write-queue job dispatch
/// (`L4a`/`L4b`) — the latter is exactly how "L4 queues are leaves" (spec 02
/// §5) becomes an enforced invariant rather than just a construction accident:
/// the writer thread marks itself as holding the hierarchy's topmost rank for
/// the job's duration, so *any* further acquisition attempted from inside the
/// job — at any level — fails the strictly-greater check.
pub fn checked_scope_sync<F, R>(level: LockLevel, body: F) -> R
where
    F: FnOnce() -> R,
{
    assert_order(level);
    CURRENT_LEVEL.sync_scope(level, body)
}

/// The async counterpart of [`checked_scope_sync`]: `fut` may `.await`
/// arbitrarily, including across an OS-thread migration on a multi-threaded
/// runtime — task-local storage follows the task, not the thread.
pub async fn checked_scope_async<Fut>(level: LockLevel, fut: Fut) -> Fut::Output
where
    Fut: Future,
{
    assert_order(level);
    CURRENT_LEVEL.scope(level, fut).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_order_allows_strictly_increasing_ranks() {
        assert!(check_order(None, LockLevel::L0).is_ok());
        assert!(check_order(Some(LockLevel::L0), LockLevel::L1).is_ok());
        assert!(check_order(Some(LockLevel::L1), LockLevel::L2Write).is_ok());
        assert!(check_order(Some(LockLevel::L2Write), LockLevel::L3).is_ok());
        assert!(check_order(Some(LockLevel::L3), LockLevel::L4a).is_ok());
    }

    #[test]
    fn check_order_rejects_same_or_lower_rank() {
        let err = check_order(Some(LockLevel::L2Write), LockLevel::L1).unwrap_err();
        assert_eq!(err.held, LockLevel::L2Write);
        assert_eq!(err.attempted, LockLevel::L1);

        // Siblings (same rank) are mutually exclusive too — L4a/L4b are two
        // independent queues, not two independently orderable levels.
        let err = check_order(Some(LockLevel::L4a), LockLevel::L4b).unwrap_err();
        assert_eq!(err.held, LockLevel::L4a);
        assert_eq!(err.attempted, LockLevel::L4b);
    }

    #[tokio::test]
    async fn checked_scope_async_nests_correctly() {
        assert_eq!(held_level(), None);
        checked_scope_async(LockLevel::L2Write, async {
            assert_eq!(held_level(), Some(LockLevel::L2Write));
            checked_scope_async(LockLevel::L4a, async {
                assert_eq!(held_level(), Some(LockLevel::L4a));
            })
            .await;
            // The inner scope's exit restores the outer level, not `None`.
            assert_eq!(held_level(), Some(LockLevel::L2Write));
        })
        .await;
        assert_eq!(held_level(), None);
    }

    #[test]
    #[should_panic(expected = "spec 02 §5 requires strictly increasing rank")]
    fn checked_scope_sync_panics_on_reverse_order() {
        checked_scope_sync(LockLevel::L2Write, || {
            checked_scope_sync(LockLevel::L1, || {});
        });
    }
}
