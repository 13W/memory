//! G02 gate-hardening: a schema-wide lint realizing spec 01 §5.1 enforcement
//! mechanism #2 — "a schema lint test that greps the DDL for forbidden column
//! placements".
//!
//! The per-table negative tests pin the exact columns of two tables
//! (`repository_has_no_canonical_path_column`,
//! `worktree_has_no_path_derived_identity_column`). This test instead iterates
//! **every** table in a freshly-migrated `state.sqlite` and asserts the two
//! structural rules from spec 01 §5.1 hold across the whole schema, so any future
//! migration that violates them is caught automatically (including the
//! content-shared tables that arrive in groups 03/11):
//!
//! 1. **No durable ID is derived from a filesystem path.** No `FOREIGN KEY`
//!    targets a path-bearing or fingerprint column — "a path-derived hash is
//!    permitted only as a *lookup key* (`worktree_path.path_fingerprint`), never
//!    as an FK target for durable state" (spec 01 §5.1).
//! 2. **Path columns are confined to the path-ledger tables.** Only
//!    `repository_path`/`worktree_path` may carry a filesystem-path column; no
//!    identity or content table does (spec 01 §5.1: path/generation-dependent data
//!    lives only in the dedicated tables).
//!
//! Deterministic: an isolated [`TempHome`], the production migration set, no clock
//! or network.

use local_rag_core::paths::StoreLayout;
use local_rag_store::StateDb;
use local_rag_store::rusqlite::Connection;
use local_rag_test_support::TempHome;

/// A temporary store with an opened [`StateDb`] (runs the production migration set:
/// registry v1 + worktree v2, plus the framework bootstrap tables).
fn open_state() -> (TempHome, StateDb) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    (home, db)
}

/// The only tables permitted to carry a filesystem-path column. Spec 01 §5.1:
/// path-dependent data lives only in dedicated tables — for the registry, the two
/// `*_path` observation ledgers.
const PATH_LEDGER_TABLES: &[&str] = &["repository_path", "worktree_path"];

/// A column name that denotes a filesystem path (`path` or any `_path` suffix,
/// e.g. `observed_path`, `observed_canonical_path`, `display_path`).
///
/// `path_fingerprint` and any other `_fingerprint` column are hashes, not paths,
/// and are handled by [`is_fingerprint_column`].
fn is_path_column(name: &str) -> bool {
    name == "path" || name.ends_with("_path")
}

/// A path/remote-derived lookup hash that must never be a foreign-key target
/// (spec 01 §5.1: "permitted only as a lookup key … never as an FK target").
fn is_fingerprint_column(name: &str) -> bool {
    name.ends_with("_fingerprint")
}

/// Every user table (skipping SQLite internals), ascending by name.
fn user_tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .expect("prepare sqlite_master");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query tables")
        .collect::<Result<_, _>>()
        .expect("collect tables")
}

/// Column names of `table`.
fn columns_of(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .expect("prepare table_info");
    stmt.query_map([table], |r| r.get::<_, String>(0))
        .expect("query columns")
        .collect::<Result<_, _>>()
        .expect("collect columns")
}

/// `(from_column, to_table, to_column)` for every foreign key declared on `table`.
fn foreign_keys(conn: &Connection, table: &str) -> Vec<(String, String, String)> {
    // pragma_foreign_key_list columns: id, seq, table, from, to, on_update, …
    let mut stmt = conn
        .prepare("SELECT \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1)")
        .expect("prepare foreign_key_list");
    stmt.query_map([table], |r| {
        Ok((
            r.get::<_, String>(1)?, // from (child column)
            r.get::<_, String>(0)?, // to table (parent)
            r.get::<_, String>(2)?, // to column (parent)
        ))
    })
    .expect("query fks")
    .collect::<Result<_, _>>()
    .expect("collect fks")
}

/// Spec 01 §5.1: no durable ID is derived from a filesystem path — a path-derived
/// hash is a lookup key only, never an FK target. Assert no foreign key anywhere in
/// the schema references a path-bearing or fingerprint column.
#[tokio::test]
async fn no_foreign_key_targets_a_path_or_fingerprint_column() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    let tables = user_tables(&read);
    assert!(
        tables.iter().any(|t| t == "worktree"),
        "sanity: the migrated schema has the registry tables ({tables:?})",
    );

    for table in &tables {
        for (from, to_table, to_col) in foreign_keys(&read, table) {
            assert!(
                !is_path_column(&to_col) && !is_fingerprint_column(&to_col),
                "FK {table}.{from} -> {to_table}.{to_col} targets a path/fingerprint column; \
                 spec 01 §5.1 forbids a durable ID derived from a path (a lookup hash is never \
                 an FK target)",
            );
        }
    }
}

/// Spec 01 §5.1: path-dependent data lives only in the dedicated ledger tables.
/// Assert no table outside [`PATH_LEDGER_TABLES`] carries a filesystem-path column.
#[tokio::test]
async fn path_columns_live_only_on_ledger_tables() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    for table in user_tables(&read) {
        if PATH_LEDGER_TABLES.contains(&table.as_str()) {
            // The ledgers legitimately carry observed/display paths; their identity
            // is still the composite PK scoped to a UUID (checked by the per-table
            // tests), never a bare path.
            continue;
        }
        for col in columns_of(&read, &table) {
            assert!(
                !is_path_column(&col),
                "table `{table}` carries path column `{col}`, but a filesystem path may live \
                 only on {PATH_LEDGER_TABLES:?} (spec 01 §5.1)",
            );
        }
    }
}

/// Guard the guard: the classifiers must actually distinguish paths, fingerprints,
/// and plain identity columns — otherwise the two lints above would be vacuous.
#[test]
fn column_classifiers_discriminate() {
    // Path columns.
    for name in [
        "path",
        "observed_path",
        "observed_canonical_path",
        "display_path",
    ] {
        assert!(is_path_column(name), "{name} is a path column");
        assert!(!is_fingerprint_column(name), "{name} is not a fingerprint");
    }
    // Fingerprints (lookup hashes) are not paths.
    for name in ["path_fingerprint", "git_remote_fingerprint"] {
        assert!(is_fingerprint_column(name), "{name} is a fingerprint");
        assert!(!is_path_column(name), "{name} is not a path column");
    }
    // Identity/plain columns are neither.
    for name in ["repo_id", "worktree_id", "generation_id", "kind", "state"] {
        assert!(!is_path_column(name), "{name} is not a path column");
        assert!(!is_fingerprint_column(name), "{name} is not a fingerprint");
    }
}
