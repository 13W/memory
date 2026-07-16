//! `local-rag` durable storage layer.
//!
//! This crate owns the SQLite databases described in spec 03: the canonical
//! `state.sqlite` (source of truth) and, later, the rebuildable `cache.sqlite`.
//!
//! T01-02 introduces the **state open policy** and the **bounded writer**:
//!
//! - every connection to `state.sqlite` applies the normative pragmas
//!   (`journal_mode=WAL`, `foreign_keys=ON`, `synchronous=FULL`,
//!   `busy_timeout=5000`; spec 03 §2);
//! - all writes flow through a single bounded async queue feeding one writer
//!   task, so SQLite's single physical writer is respected and producers see
//!   backpressure (spec 02 §5 L4a, 03 §3);
//! - no writable [`rusqlite::Connection`] ever leaves this crate — the only way
//!   to mutate `state.sqlite` is [`StateWriter::transaction`]. Reads use a
//!   read-only connection ([`StateDb::open_read`]) that cannot write.
//!
//! T01-03 adds the **forward-only migration runner** ([`migrate`]): every
//! `StateDb::open` bootstraps `schema_migrations`/`store_settings`, checks
//! compatibility, and applies pending migrations under the migration lock (L1)
//! before the writer spawns. Resumable/destructive migrations (T01-04) and
//! `cache.sqlite` (T01-05) build on top of this.
//!
//! `rusqlite` is re-exported so downstream crates share one SQLite vocabulary
//! (`local_rag_store::rusqlite`).

pub use local_rag_core::VERSION;
pub use rusqlite;

mod clock;
pub mod migrate;
mod state;

pub use migrate::{ALL, Migration, MigrationError, MigrationReport};
pub use state::{DEFAULT_WRITE_QUEUE_CAPACITY, OpenError, StateDb, StateWriter, WriteError};
