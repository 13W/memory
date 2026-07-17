//! Per-repository settings and the effective `data_policy` merge (spec 02 §3.2,
//! 03 §2.1, 12 §1).
//!
//! `repo_settings` is a generic `(repo_id, key, value)` table (spec 03 §2.1)
//! whose keys mirror the global `[models]`/`[index]` config sections (spec 02
//! §3.2). Settings are edited via the CLI/dashboard — **never via files inside a
//! repository** (that invariant is enforced at the config layer, where the only
//! config file is the global `<config_dir>/config.toml`; see
//! [`local_rag_core::config`]).
//!
//! These follow the same shape as the other registry primitives: write
//! operations take a [`Transaction`] so they compose inside a single
//! [`StateWriter::transaction`](crate::StateWriter::transaction) closure; read
//! operations take a [`Connection`]. Every operation returns [`rusqlite::Result`];
//! setting a value for an unknown `repo_id` is rejected by the `repo_settings →
//! repository` foreign key and rolls the transaction back.
//!
//! The one typed key with merge semantics is `data_policy`
//! ([`DATA_POLICY_KEY`]). Its **effective** value for a request is the *most
//! restrictive* of the global policy and every involved repository's policy
//! (spec 02 §3.2, 12 §1 `[FIXED]`): a repository can only tighten, never relax,
//! the global policy. The central remote-policy guard that consumes this value
//! lives in the provider pool (spec 10 §1, 12 §1) and is a later group
//! (T11/T16); this module supplies only the stored values and the merge.

use local_rag_core::DataPolicy;
use rusqlite::types::Type;
use rusqlite::{Connection, Error, OptionalExtension, Transaction, params};

/// The `repo_settings` key holding a repository's `data_policy` override,
/// mirroring the global `[models].data_policy` key (spec 02 §3.2).
pub const DATA_POLICY_KEY: &str = "data_policy";

/// Set (upsert) a per-repository setting (spec 03 §2.1).
///
/// Idempotent: writing the same key twice leaves a single row and overwrites the
/// value. An unknown `repo_id` is rejected by the `repo_settings → repository`
/// foreign key (a
/// [`ConstraintViolation`](rusqlite::ErrorCode::ConstraintViolation) that rolls
/// the transaction back).
pub fn set_repo_setting(
    tx: &Transaction<'_>,
    repo_id: &str,
    key: &str,
    value: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO repo_settings (repo_id, key, value) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(repo_id, key) DO UPDATE SET value = ?3",
        params![repo_id, key, value],
    )?;
    Ok(())
}

/// Read a single per-repository setting, or `None` if it is unset (spec 03 §2.1).
pub fn get_repo_setting(
    conn: &Connection,
    repo_id: &str,
    key: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM repo_settings WHERE repo_id = ?1 AND key = ?2",
        params![repo_id, key],
        |r| r.get(0),
    )
    .optional()
}

/// All of a repository's settings as `(key, value)` pairs, ordered by key
/// (spec 03 §2.1).
///
/// The deterministic `ORDER BY key` makes a merged snapshot reproducible.
pub fn repo_settings(conn: &Connection, repo_id: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT key, value FROM repo_settings WHERE repo_id = ?1 ORDER BY key")?;
    let rows = stmt.query_map(params![repo_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Set a repository's `data_policy` override (spec 02 §3.2).
///
/// Stores the canonical string form ([`DataPolicy::as_str`]) under
/// [`DATA_POLICY_KEY`]. A repository policy only ever *tightens* the effective
/// policy (see [`effective_data_policy`]).
pub fn set_repo_data_policy(
    tx: &Transaction<'_>,
    repo_id: &str,
    policy: DataPolicy,
) -> rusqlite::Result<()> {
    set_repo_setting(tx, repo_id, DATA_POLICY_KEY, policy.as_str())
}

/// Read a repository's `data_policy` override, or `None` if it is unset
/// (spec 02 §3.2).
///
/// A stored value outside the four canonical names is corruption and surfaces as
/// [`Error::FromSqlConversionFailure`] (the same idiom the worktree state machine
/// uses for an out-of-domain stored enum), never a silent default.
pub fn repo_data_policy(conn: &Connection, repo_id: &str) -> rusqlite::Result<Option<DataPolicy>> {
    let raw: Option<String> = get_repo_setting(conn, repo_id, DATA_POLICY_KEY)?;
    match raw {
        None => Ok(None),
        Some(value) => DataPolicy::from_str_value(&value).map(Some).ok_or_else(|| {
            Error::FromSqlConversionFailure(
                0,
                Type::Text,
                format!("invalid repo_settings.data_policy {value:?}").into(),
            )
        }),
    }
}

/// The effective `data_policy` for a request spanning `repo_ids` (spec 02 §3.2,
/// 12 §1 `[FIXED]`).
///
/// The result is the *most restrictive* of `global` and every involved
/// repository's stored policy; a repository without a `data_policy` setting does
/// not affect the result. Because [`DataPolicy::most_restrictive`] is commutative
/// and associative, the fold is order-independent — the merged snapshot is
/// deterministic regardless of the order of `repo_ids` — and a repository can only
/// tighten, never relax, the global policy.
///
/// A corrupt stored value for any repository propagates as
/// [`Error::FromSqlConversionFailure`] (via [`repo_data_policy`]).
pub fn effective_data_policy(
    global: DataPolicy,
    conn: &Connection,
    repo_ids: &[&str],
) -> rusqlite::Result<DataPolicy> {
    let mut effective = global;
    for repo_id in repo_ids {
        if let Some(repo_policy) = repo_data_policy(conn, repo_id)? {
            effective = effective.most_restrictive(repo_policy);
        }
    }
    Ok(effective)
}
