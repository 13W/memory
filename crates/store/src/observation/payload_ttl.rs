//! The `observation_payload` TTL sweep (spec 12 §3 "Retention": "`observation_payload`
//! under real TTL (`payload_ttl_hours`), enforced by a sweeper; envelopes are
//! durable"). T13-05.
//!
//! `observation_payload.expires_at` is already computed at import time
//! ([`super::import::import_batch`], T13-04) from
//! `local_rag_core::config::StorageConfig::payload_ttl_hours`; this sweep only
//! removes rows past that deadline. `observation_envelope` and
//! `observation_path` are never touched here — their survival is structural,
//! not a sweep decision: a payload row's absence *is* "no payload" (either it
//! never had one, or it expired), never distinguishable from the outside, by
//! design (spec 03 §2.5's `observation_payload` doc: "short TTL; envelope
//! survives it").

use rusqlite::params;

use crate::state::{OpenError, StateDb, WriteError};

/// The outcome of one [`run_payload_ttl_sweep`] pass — includes the "envelope
/// metrics" the card names: `total_envelopes` and `payload_retained` describe
/// the store's observation ledger, not just what this sweep touched, so a
/// caller can watch the payload/envelope ratio over time (e.g. via a future
/// CLI/status surface, group 15) without a second query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PayloadSweepReport {
    /// `observation_payload` rows deleted (or, for a dry run, that would be).
    pub payload_removed: u64,
    /// `observation_payload` rows still present (within TTL) after this sweep.
    pub payload_retained: u64,
    /// Total `observation_envelope` rows in the store — unaffected by this
    /// sweep, reported as a metric for the payload/envelope ratio.
    pub total_envelopes: u64,
    /// Whether this was a dry run (nothing was actually deleted).
    pub dry_run: bool,
}

/// A failure from [`run_payload_ttl_sweep`].
#[derive(Debug)]
#[non_exhaustive]
pub enum PayloadSweepError {
    /// Opening the read-only state connection (dry-run path) failed.
    Open(OpenError),
    /// The sweep transaction failed (rolled back; the store is unchanged).
    Write(WriteError),
}

impl std::fmt::Display for PayloadSweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PayloadSweepError::Open(e) => write!(f, "could not open the state store: {e}"),
            PayloadSweepError::Write(e) => write!(f, "payload TTL sweep failed: {e}"),
        }
    }
}

impl std::error::Error for PayloadSweepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PayloadSweepError::Open(e) => Some(e),
            PayloadSweepError::Write(e) => Some(e),
        }
    }
}

/// Delete every `observation_payload` row whose `expires_at <= now_ms` (the
/// `>=`-at-the-deadline convention `housekeeping::shard_destroy_due` already
/// established: a TTL that expires exactly now means "remove now", not
/// "next sweep"). `dry_run` only reads (no transaction, no mutation).
///
/// Idempotent: a repeated sweep after the first finds no more expired rows
/// and reports `payload_removed: 0`.
pub async fn run_payload_ttl_sweep(
    db: &StateDb,
    now_ms: i64,
    dry_run: bool,
) -> Result<PayloadSweepReport, PayloadSweepError> {
    if dry_run {
        let read = db.open_read().map_err(PayloadSweepError::Open)?;
        let payload_removed: i64 = read
            .query_row(
                "SELECT count(*) FROM observation_payload WHERE expires_at <= ?1",
                params![now_ms],
                |r| r.get(0),
            )
            .map_err(|e| PayloadSweepError::Write(WriteError::Sqlite(e)))?;
        let payload_retained: i64 = read
            .query_row(
                "SELECT count(*) FROM observation_payload WHERE expires_at > ?1",
                params![now_ms],
                |r| r.get(0),
            )
            .map_err(|e| PayloadSweepError::Write(WriteError::Sqlite(e)))?;
        let total_envelopes: i64 = read
            .query_row("SELECT count(*) FROM observation_envelope", [], |r| {
                r.get(0)
            })
            .map_err(|e| PayloadSweepError::Write(WriteError::Sqlite(e)))?;
        return Ok(PayloadSweepReport {
            payload_removed: payload_removed as u64,
            payload_retained: payload_retained as u64,
            total_envelopes: total_envelopes as u64,
            dry_run: true,
        });
    }

    db.writer()
        .transaction(move |tx| {
            let payload_removed = tx.execute(
                "DELETE FROM observation_payload WHERE expires_at <= ?1",
                params![now_ms],
            )?;
            let payload_retained: i64 =
                tx.query_row("SELECT count(*) FROM observation_payload", [], |r| r.get(0))?;
            let total_envelopes: i64 =
                tx.query_row("SELECT count(*) FROM observation_envelope", [], |r| {
                    r.get(0)
                })?;
            Ok(PayloadSweepReport {
                payload_removed: payload_removed as u64,
                payload_retained: payload_retained as u64,
                total_envelopes: total_envelopes as u64,
                dry_run: false,
            })
        })
        .await
        .map_err(PayloadSweepError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_core::paths::StoreLayout;
    use local_rag_test_support::TempHome;

    fn open_state() -> (TempHome, StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, db)
    }

    /// Insert one envelope (+ path + payload with the given `expires_at`)
    /// directly, bypassing `import_batch` — this module only cares about
    /// `observation_payload`/`observation_envelope` rows existing, not how
    /// they got there.
    fn seed(
        tx: &rusqlite::Transaction<'_>,
        observation_id: &str,
        session_id: &str,
        expires_at: i64,
    ) -> rusqlite::Result<()> {
        super::super::insert_envelope(
            tx,
            &super::super::NewObservationEnvelope {
                observation_id,
                source_event_id: observation_id,
                dedup_key: None,
                payload_hash: "deadbeef",
                event_type: "Stop",
                evidence_kind: "model_claim",
                trust: "low",
                source_timestamp: Some(0),
                repo_id: None,
                worktree_id: None,
                session_id,
                agent_id: None,
                turn_id: None,
                batch_id: None,
                commit_hash: None,
                short_evidence_excerpt: None,
                redaction_version: None,
            },
        )?;
        super::super::insert_path(tx, observation_id, "src/a.rs")?;
        super::super::insert_payload(tx, observation_id, b"{}", expires_at)?;
        Ok(())
    }

    #[tokio::test]
    async fn fake_clock_before_at_and_after_ttl() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| seed(tx, "obs-1", "sess-1", 1_000))
            .await
            .unwrap();

        // Before the deadline: retained.
        let report = run_payload_ttl_sweep(&db, 999, false).await.unwrap();
        assert_eq!(report.payload_removed, 0);
        assert_eq!(report.payload_retained, 1);

        // Exactly at the deadline: removed (`<=`, not `<`).
        let report = run_payload_ttl_sweep(&db, 1_000, false).await.unwrap();
        assert_eq!(report.payload_removed, 1);
        assert_eq!(report.payload_retained, 0);
    }

    #[tokio::test]
    async fn after_ttl_is_also_removed() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| seed(tx, "obs-1", "sess-1", 1_000))
            .await
            .unwrap();

        let report = run_payload_ttl_sweep(&db, 5_000, false).await.unwrap();
        assert_eq!(report.payload_removed, 1);
    }

    #[tokio::test]
    async fn evidence_survives_payload_removal() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| seed(tx, "obs-1", "sess-1", 1_000))
            .await
            .unwrap();

        run_payload_ttl_sweep(&db, 5_000, false).await.unwrap();

        let read = db.open_read().unwrap();
        let envelopes: i64 = read
            .query_row("SELECT count(*) FROM observation_envelope", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(envelopes, 1, "the envelope survives payload expiry");
        let paths: i64 = read
            .query_row("SELECT count(*) FROM observation_path", [], |r| r.get(0))
            .unwrap();
        assert_eq!(paths, 1, "observation_path survives payload expiry too");
        let payloads: i64 = read
            .query_row("SELECT count(*) FROM observation_payload", [], |r| r.get(0))
            .unwrap();
        assert_eq!(payloads, 0);
    }

    #[tokio::test]
    async fn repeated_sweep_is_idempotent() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| seed(tx, "obs-1", "sess-1", 1_000))
            .await
            .unwrap();

        let first = run_payload_ttl_sweep(&db, 5_000, false).await.unwrap();
        assert_eq!(first.payload_removed, 1);
        let second = run_payload_ttl_sweep(&db, 5_000, false).await.unwrap();
        assert_eq!(second.payload_removed, 0, "nothing left to remove");
    }

    #[tokio::test]
    async fn dry_run_reports_without_removing() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| seed(tx, "obs-1", "sess-1", 1_000))
            .await
            .unwrap();

        let report = run_payload_ttl_sweep(&db, 5_000, true).await.unwrap();
        assert_eq!(report.payload_removed, 1);
        assert!(report.dry_run);

        let read = db.open_read().unwrap();
        let payloads: i64 = read
            .query_row("SELECT count(*) FROM observation_payload", [], |r| r.get(0))
            .unwrap();
        assert_eq!(payloads, 1, "dry run must not delete");
    }

    #[tokio::test]
    async fn envelope_metrics_reflect_a_mixed_expired_and_live_set() {
        let (_home, db) = open_state();
        db.writer()
            .transaction(|tx| {
                seed(tx, "obs-expired-1", "sess-1", 1_000)?;
                seed(tx, "obs-expired-2", "sess-1", 2_000)?;
                seed(tx, "obs-live", "sess-1", 9_000)?;
                Ok(())
            })
            .await
            .unwrap();

        let report = run_payload_ttl_sweep(&db, 5_000, false).await.unwrap();
        assert_eq!(report.payload_removed, 2);
        assert_eq!(report.payload_retained, 1);
        assert_eq!(
            report.total_envelopes, 3,
            "envelopes are never removed by this sweep"
        );
    }
}
