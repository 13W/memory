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
//!    targets a path-derived *hash* — "a path-derived hash is permitted only as a
//!    *lookup key* (`worktree_path.path_fingerprint`), never as an FK target for
//!    durable state" (spec 01 §5.1). A plain `normalized_path` is *not* a
//!    path-derived hash: it is the path itself used as a generation-membership
//!    natural key, and spec 03 §2.4 normatively declares exactly one FK through it
//!    — `generation_unit_occurrence(generation_id, normalized_path) →
//!    generation_file(...)`, the structural source-blob invariant. So an FK may
//!    target a path column *only* on the sanctioned membership anchor
//!    ([`PATH_FK_TARGET_TABLES`]); a `*_fingerprint` target is forbidden outright.
//! 2. **Path columns are confined to the path-bearing tables.** Only the two
//!    path-observation ledgers ([`PATH_LEDGER_TABLES`]) and the generation-membership
//!    tables ([`PATH_MEMBERSHIP_TABLES`]) may carry a filesystem-path column; no
//!    content-shared or identity table does (spec 01 §5.1: "Everything
//!    path/generation-dependent lives only in `generation_unit_occurrence`,
//!    `resolved_graph_edge`, `generation_file`, and the FTS projection").
//! 3. **Content-shared rows carry no context/path/generation field**
//!    ([`CONTENT_SHARED_TABLES`]): the §2.3 tables are shared by content across
//!    every path and generation, so they must not carry any path-, context-, or
//!    generation-specific column (spec 01 §5.1) — the "schema audit forbidden path
//!    columns" guardrail T03-01 owns.
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

/// The two path-observation ledgers. Spec 01 §5.1: path-dependent data lives only
/// in dedicated tables — for the registry, these `*_path` observation ledgers.
const PATH_LEDGER_TABLES: &[&str] = &["repository_path", "worktree_path"];

/// The generation-membership tables that legitimately carry a `normalized_path`/
/// `display_path` (spec 03 §2.4; spec 01 §5.1 names `generation_unit_occurrence`,
/// `resolved_graph_edge`, `generation_file` — plus `skipped_file`, the
/// path-keyed skip ledger, per 06 §2.2 / 12 §5). Only the three that actually
/// carry a path column are listed; `resolved_graph_edge` is occurrence-id keyed.
const PATH_MEMBERSHIP_TABLES: &[&str] = &[
    "generation_file",
    "skipped_file",
    "generation_unit_occurrence",
];

/// The content-shared, path-independent tables (spec 03 §2.3): a row here is
/// shared by content across every path and generation, so it must carry **no**
/// path-, context-, or generation-specific field (spec 01 §5.1).
const CONTENT_SHARED_TABLES: &[&str] = &["file_revision", "content_blob", "parsed_unit"];

/// The only tables a foreign key may target *through a path column*: the
/// generation-membership anchor whose primary key is `(generation_id,
/// normalized_path)`. Spec 03 §2.4 declares exactly this composite FK from
/// `generation_unit_occurrence`; nothing else may FK-target a path column, and a
/// path-derived hash (`*_fingerprint`) is never a legal FK target (spec 01 §5.1).
const PATH_FK_TARGET_TABLES: &[&str] = &["generation_file"];

/// Whether `table` is permitted to carry a filesystem-path column (a ledger or a
/// generation-membership table).
fn may_carry_path(table: &str) -> bool {
    PATH_LEDGER_TABLES.contains(&table) || PATH_MEMBERSHIP_TABLES.contains(&table)
}

/// A context/generation-specific column forbidden on a content-shared table
/// (spec 01 §5.1). Path columns are caught separately by [`is_path_column`].
fn is_context_or_generation_column(name: &str) -> bool {
    matches!(
        name,
        "generation_id" | "worktree_id" | "context_hash" | "qualified_name"
    )
}

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
/// *hash* is a lookup key only, never an FK target. A plain `normalized_path` may
/// be an FK target only on the sanctioned generation-membership anchor
/// ([`PATH_FK_TARGET_TABLES`]), the one composite FK spec 03 §2.4 declares.
#[tokio::test]
async fn no_foreign_key_targets_a_path_hash_or_stray_path_column() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    let tables = user_tables(&read);
    assert!(
        tables.iter().any(|t| t == "worktree"),
        "sanity: the migrated schema has the registry tables ({tables:?})",
    );

    for table in &tables {
        for (from, to_table, to_col) in foreign_keys(&read, table) {
            // A path-derived hash is NEVER an FK target (spec 01 §5.1).
            assert!(
                !is_fingerprint_column(&to_col),
                "FK {table}.{from} -> {to_table}.{to_col} targets a path-derived fingerprint \
                 hash; spec 01 §5.1 forbids a durable ID derived from a path (a lookup hash is \
                 never an FK target)",
            );
            // A plain path column may be an FK target only on the sanctioned
            // membership anchor (spec 03 §2.4's structural source-blob invariant).
            if is_path_column(&to_col) {
                assert!(
                    PATH_FK_TARGET_TABLES.contains(&to_table.as_str()),
                    "FK {table}.{from} -> {to_table}.{to_col} targets a path column on a table \
                     that is not a sanctioned generation-membership anchor \
                     ({PATH_FK_TARGET_TABLES:?}); spec 01 §5.1 / 03 §2.4",
                );
            }
        }
    }
}

/// Spec 01 §5.1: path-dependent data lives only in dedicated tables. Assert no
/// table outside the path ledgers or the generation-membership tables carries a
/// filesystem-path column.
#[tokio::test]
async fn path_columns_live_only_on_path_bearing_tables() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    for table in user_tables(&read) {
        if may_carry_path(&table) {
            // Ledgers carry observed/display paths; membership tables carry a
            // `normalized_path`/`display_path` scoped to a generation. Their
            // identity is still a composite key (checked elsewhere), never a
            // bare path, and no path-derived hash is an FK target.
            continue;
        }
        for col in columns_of(&read, &table) {
            assert!(
                !is_path_column(&col),
                "table `{table}` carries path column `{col}`, but a filesystem path may live \
                 only on {PATH_LEDGER_TABLES:?} or {PATH_MEMBERSHIP_TABLES:?} (spec 01 §5.1)",
            );
        }
    }
}

/// Spec 01 §5.1: "No row that is shared by content … may carry any context- or
/// path-specific field." Assert the §2.3 content-shared tables carry no path-,
/// context-, or generation-specific column — the "schema audit forbidden path
/// columns" guardrail T03-01 owns.
#[tokio::test]
async fn content_shared_tables_carry_no_path_or_context_field() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    let tables = user_tables(&read);
    for shared in CONTENT_SHARED_TABLES {
        assert!(
            tables.iter().any(|t| t == shared),
            "sanity: content-shared table `{shared}` exists in the migrated schema ({tables:?})",
        );
        for col in columns_of(&read, shared) {
            assert!(
                !is_path_column(&col) && !is_context_or_generation_column(&col),
                "content-shared table `{shared}` carries forbidden path/context/generation \
                 column `{col}`; spec 01 §5.1 forbids any path-/context-specific field on a \
                 content-shared row",
            );
        }
    }
}

/// Spec 03 §2.3/§4.2: a `content_blob` row carries identity + metadata only; the
/// normalized text "lives in the cache" (`normalized_text_cache`). Assert **no**
/// `state.sqlite` table stores a `normalized_text` column — the "no normalized
/// text stored in path-bearing/canonical code rows" guardrail T03-04 owns. The
/// only home for normalized text is the rebuildable cache (checked in
/// `tests/normalized_text.rs`).
#[tokio::test]
async fn no_state_table_stores_normalized_text() {
    let (_home, db) = open_state();
    let read = db.open_read().expect("read conn");

    let tables = user_tables(&read);
    assert!(
        tables.iter().any(|t| t == "content_blob"),
        "sanity: content_blob exists in the migrated schema ({tables:?})",
    );
    for table in &tables {
        for col in columns_of(&read, table) {
            assert_ne!(
                col, "normalized_text",
                "table `{table}` stores `normalized_text`, but normalized text is canonical only \
                 in the rebuildable `normalized_text_cache` (spec 03 §2.3/§4.2, T03-04)",
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
        "normalized_path",
    ] {
        assert!(is_path_column(name), "{name} is a path column");
        assert!(!is_fingerprint_column(name), "{name} is not a fingerprint");
        assert!(
            !is_context_or_generation_column(name),
            "{name} is a path column, classified separately from context/generation columns",
        );
    }
    // Context/generation-specific columns forbidden on content-shared tables.
    for name in [
        "generation_id",
        "worktree_id",
        "context_hash",
        "qualified_name",
    ] {
        assert!(
            is_context_or_generation_column(name),
            "{name} is a context/generation column",
        );
        assert!(!is_path_column(name), "{name} is not a path column");
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
