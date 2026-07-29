//! The store's own random identity (`store_settings.store_instance_uuid`,
//! spec 02 §2/§4.1) — T15-01.
//!
//! `cache.sqlite`'s own binding (`crate::cache::CacheDb::open`) already
//! *consumes* a `store_instance_uuid`; nothing before this task ever produced
//! one. T01-05's as-built note on `SCHEMA_V1`/`SCHEMA_V2` bootstrap flagged
//! this precisely: seeding it was "deferred to... daemon wiring (step 15)".
//! This is also the value the daemon's `store.lock` JSON (02 §2) and the live
//! liveness handshake (02 §4.1's "instance UUID matches a live handshake on
//! the socket") both key off — but that `instance_uuid` is a **process**
//! identity, freshly random on every daemon start, and deliberately a
//! *different* value from this module's **store** identity, which must
//! survive restarts (it is what binds `cache.sqlite` to `state.sqlite` across
//! them). Do not conflate the two.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// The `store_settings` key under which the store's identity is recorded.
const STORE_INSTANCE_UUID_KEY: &str = "store_instance_uuid";

/// The store's `store_instance_uuid`, if one has ever been recorded.
pub fn store_instance_uuid(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM store_settings WHERE key = ?1",
        params![STORE_INSTANCE_UUID_KEY],
        |r| r.get(0),
    )
    .optional()
}

/// Ensure `store_settings.store_instance_uuid` is set, returning the value now
/// on record.
///
/// `candidate` is minted by the caller (a fresh UUIDv7) **before** this call,
/// the same "entropy stays out of the write path" discipline
/// [`create_repository`](super::create_repository)'s own caller already
/// follows — a migration/transaction body has no clock or entropy source of
/// its own.
///
/// Idempotent and race-free in one atomic statement, the same `ON CONFLICT
/// ... DO UPDATE ... RETURNING` idiom
/// [`register_representation`](super::register_representation) already uses:
/// `RETURNING` only fires for a row actually inserted or updated, never for a
/// skipped conflict, so a plain `DO NOTHING` would not hand back the
/// pre-existing value. The **first** daemon to ever open this store wins;
/// every later open (including a concurrent stale-lock race that briefly saw
/// two candidate daemons) converges on that same value and `candidate` is
/// discarded on that path.
pub fn ensure_store_instance_uuid(
    tx: &Transaction<'_>,
    candidate: &str,
) -> rusqlite::Result<String> {
    tx.query_row(
        "INSERT INTO store_settings (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = store_settings.value \
         RETURNING value",
        params![STORE_INSTANCE_UUID_KEY, candidate],
        |r| r.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A bare `store_settings` table, matching `crate::migrate`'s bootstrap DDL
    /// (created unconditionally on every open, outside the numbered set) —
    /// this module needs nothing else from a real store.
    fn conn_with_store_settings() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE store_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn unset_on_a_freshly_bootstrapped_store() {
        let conn = conn_with_store_settings();
        assert_eq!(store_instance_uuid(&conn).unwrap(), None);
    }

    #[test]
    fn ensure_sets_it_once_and_is_idempotent() {
        let mut conn = conn_with_store_settings();
        let tx = conn.transaction().unwrap();
        let first = ensure_store_instance_uuid(&tx, "candidate-a").unwrap();
        assert_eq!(first, "candidate-a");
        tx.commit().unwrap();

        assert_eq!(
            store_instance_uuid(&conn).unwrap().as_deref(),
            Some("candidate-a")
        );

        // A second candidate (e.g. a later daemon start) must NOT overwrite it.
        let tx = conn.transaction().unwrap();
        let second = ensure_store_instance_uuid(&tx, "candidate-b").unwrap();
        assert_eq!(second, "candidate-a", "first writer wins");
        tx.commit().unwrap();
        assert_eq!(
            store_instance_uuid(&conn).unwrap().as_deref(),
            Some("candidate-a")
        );
    }
}
