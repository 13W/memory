//! Shared offline-safe **write-capable** `state.sqlite` open helper (T18-05) — the write-side
//! counterpart to `store_read.rs::open_read_offline_safe`. Same diagnose-before-open precaution
//! (via `store_read::diagnose_ready`, so the two paths cannot drift in wording), returning the
//! owned `StateDb` (hence `.writer()`) instead of a bare read `Connection`. A second write caller
//! is expected at T18-06/T18-07 (Repo Settings/Server Settings), the reason this lives in its own
//! sibling module rather than inlined in `memory.rs`.

use local_rag_core::paths::StoreLayout;
use local_rag_store::StateDb;

/// Open a write-capable `StateDb`, never applying a pending migration as a side effect (see
/// `store_read`'s own module doc for the full rationale — a mutation action is a deliberate user
/// choice, but this dashboard treats every screen's own store access uniformly rather than
/// letting some keypresses silently migrate the store and others refuse to).
pub fn open_write_offline_safe(layout: &StoreLayout) -> Result<StateDb, String> {
    crate::store_read::diagnose_ready(layout)?;
    StateDb::open(layout.state_db()).map_err(|e| format!("could not open state.sqlite: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_core::paths::StoreLayout;
    use local_rag_test_support::TempHome;

    #[test]
    fn refuses_before_the_store_is_ever_initialized() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");

        let err = open_write_offline_safe(&layout).expect_err("no state.sqlite yet");
        assert!(err.contains("not yet initialized"), "{err}");
    }

    #[test]
    fn opens_once_the_store_is_applied() {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        StateDb::open(layout.state_db()).expect("bootstrap state.sqlite");

        open_write_offline_safe(&layout).expect("already-applied store opens for writing");
    }
}
