//! T16-03 acceptance tests for `CacheDb::diagnose_binding` (spec 11 §6, `local-rag
//! doctor`'s own check): read-only across every binding-mismatch scenario
//! `open_and_bind` already treats as "rebuild", but here it must never
//! actually rebuild anything.
//!
//! All tests are deterministic: isolated [`TempHome`], no wall-clock sleeps.

use std::path::Path;

use local_rag_core::paths::StoreLayout;
use local_rag_store::rusqlite::params;
use local_rag_store::{CACHE_SCHEMA_VERSION, CacheDb, CacheDiagnosis};
use local_rag_test_support::TempHome;

const UUID_A: &str = "11111111-1111-7111-8111-111111111111";
const UUID_B: &str = "22222222-2222-7222-8222-222222222222";

fn temp_store() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn open_cache(layout: &StoreLayout, uuid: &str) -> CacheDb {
    CacheDb::open(layout.cache_db(), uuid).expect("open cache.sqlite")
}

#[test]
fn diagnose_cache_binding_reports_not_initialized_when_cache_sqlite_is_absent() {
    let (_home, layout) = temp_store();
    let diagnosis = CacheDb::diagnose_binding(&layout.cache_db(), UUID_A);
    assert_eq!(diagnosis, CacheDiagnosis::NotInitialized);
    assert!(
        !layout.cache_db().exists(),
        "diagnose_binding must not create the file it is diagnosing"
    );
}

#[test]
fn diagnose_cache_binding_reports_unreadable_on_a_corrupt_file() {
    let (_home, layout) = temp_store();
    std::fs::write(layout.cache_db(), b"not a sqlite database").expect("seed garbage file");

    let diagnosis = CacheDb::diagnose_binding(&layout.cache_db(), UUID_A);
    assert_eq!(diagnosis, CacheDiagnosis::Unreadable);
}

#[test]
fn diagnose_cache_binding_reports_bound_on_a_healthy_cache() {
    let (_home, layout) = temp_store();
    let cache = open_cache(&layout, UUID_A);
    cache.close();

    let diagnosis = CacheDb::diagnose_binding(&layout.cache_db(), UUID_A);
    assert_eq!(diagnosis, CacheDiagnosis::Bound);
}

#[test]
fn diagnose_cache_binding_reports_wrong_binding_after_store_instance_uuid_changes() {
    let (_home, layout) = temp_store();
    let cache = open_cache(&layout, UUID_A);
    cache.close();

    let diagnosis = CacheDb::diagnose_binding(&layout.cache_db(), UUID_B);
    assert_eq!(
        diagnosis,
        CacheDiagnosis::WrongBinding {
            found: UUID_A.to_string()
        }
    );
}

#[test]
fn diagnose_cache_binding_reports_incompatible_schema() {
    let (_home, layout) = temp_store();
    let cache = open_cache(&layout, UUID_A);
    cache.close();

    // Directly tamper with the recorded schema version -- the same "someone
    // else wrote an old/foreign value" scenario `open_and_bind` treats as
    // "rebuild".
    let conn = local_rag_store::rusqlite::Connection::open(layout.cache_db()).expect("raw rw conn");
    conn.execute(
        "UPDATE cache_meta SET value = ?1 WHERE key = 'cache_schema_version'",
        params![(CACHE_SCHEMA_VERSION - 1).to_string()],
    )
    .expect("tamper with schema version");
    drop(conn);

    let diagnosis = CacheDb::diagnose_binding(&layout.cache_db(), UUID_A);
    assert_eq!(
        diagnosis,
        CacheDiagnosis::IncompatibleSchema {
            found: CACHE_SCHEMA_VERSION - 1,
            binary: CACHE_SCHEMA_VERSION,
        }
    );
}

#[test]
fn diagnose_cache_binding_never_rebuilds_an_incompatible_cache() {
    let (_home, layout) = temp_store();
    let cache = open_cache(&layout, UUID_A);
    cache.close();

    fn read_bytes(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("read cache.sqlite bytes")
    }
    let before = read_bytes(&layout.cache_db());

    // Diagnose against a *different* store -- exactly the condition
    // `open_and_bind` would rebuild on.
    let diagnosis = CacheDb::diagnose_binding(&layout.cache_db(), UUID_B);
    assert_eq!(
        diagnosis,
        CacheDiagnosis::WrongBinding {
            found: UUID_A.to_string()
        }
    );

    let after = read_bytes(&layout.cache_db());
    assert_eq!(
        before, after,
        "diagnose_binding must never rebuild/modify an incompatible cache file"
    );
}
