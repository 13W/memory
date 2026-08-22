//! D-090: shutdown must not release the store lock while this process can
//! still write the store.
//!
//! The measured defect (`logs/daemon.2026-08-21.log`): `daemon stopped` at
//! 23:33:32.620 for pid 51345, `store lock acquired pid=92324` 269 ms later,
//! and pid 51345 still `R` at 40% CPU with 11 open `state.sqlite`
//! descriptors three minutes after that. Nothing about the lock file was
//! corrupt and nothing reclaimed it — the outgoing daemon simply released it
//! while an abandoned indexing thread was still running, because
//! `drain_and_shutdown` released unconditionally.
//!
//! `tests/checkpoint_shutdown.rs` already proves the drained case (the lock
//! *is* released after an orderly shutdown). This file is its mirror: the
//! same call, told that a worker was cancelled rather than joined, must leave
//! the lock exactly where it is — the same file, the same inode, still
//! `flock`'d — so a second daemon cannot start alongside a process that has
//! not stopped writing.

#![cfg(unix)]

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use local_rag::daemon::{StoreLockError, WorkersDrained, acquire, drain_and_shutdown};
use local_rag_core::paths::StoreLayout;
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn lock_inode(layout: &StoreLayout) -> Option<u64> {
    std::fs::metadata(layout.store_lock()).ok().map(|m| m.ino())
}

/// Zero handover budget throughout: D-084's rule is that a held lock must be
/// refused *now*, and any retry here would blur the two outcomes this file
/// exists to tell apart.
fn try_acquire_now(
    layout: &StoreLayout,
) -> Result<local_rag::daemon::StoreLockGuard, StoreLockError> {
    acquire(layout, "second-daemon", 999, "0.0.0", 5_000, Duration::ZERO)
}

#[tokio::test]
async fn a_drain_that_abandoned_a_worker_keeps_the_lock() {
    let (_home, layout) = open_layout();
    let guard = acquire(&layout, "outgoing", 4242, "0.0.0", 1_000, Duration::ZERO)
        .expect("the store is free");
    let before = lock_inode(&layout).expect("the lock file exists while held");

    drain_and_shutdown(&layout, None, None, guard, None, WorkersDrained::No).await;

    assert!(
        layout.store_lock().exists(),
        "the record must stay: if the path is unlinked while the descriptor is kept, the next \
         acquire creates a new inode and locks that instead"
    );
    assert_eq!(
        lock_inode(&layout),
        Some(before),
        "the file at the path must still be the very one the guard holds open (D-065's idiom)"
    );
    match try_acquire_now(&layout) {
        Err(StoreLockError::Locked { owner }) => {
            assert_eq!(
                owner.pid, 4242,
                "the refusal must name the process still running"
            );
        }
        other => panic!(
            "a second daemon must not be able to take a store whose previous owner may still be \
             writing it: {other:?}"
        ),
    }
}

#[tokio::test]
async fn a_drain_that_joined_every_worker_releases_the_lock() {
    let (_home, layout) = open_layout();
    let guard = acquire(&layout, "outgoing", 4242, "0.0.0", 1_000, Duration::ZERO)
        .expect("the store is free");
    assert!(lock_inode(&layout).is_some());

    drain_and_shutdown(&layout, None, None, guard, None, WorkersDrained::Yes).await;

    assert!(
        !layout.store_lock().exists(),
        "an orderly shutdown must leave nothing behind"
    );
    let reacquired = try_acquire_now(&layout);
    assert!(
        reacquired.is_ok(),
        "the lock must be free immediately, with no recovery branch needed: {reacquired:?}"
    );
    if let Ok(guard) = reacquired {
        guard.release(&layout);
    }
}
