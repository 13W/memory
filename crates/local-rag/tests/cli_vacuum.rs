//! `local-rag vacuum [--dry-run]` acceptance tests (spec 11 §6, `X-012`).
//!
//! What is deliberately **not** tested here, and why: that the command reports
//! a bloated store. The predicate's floor is one gibibyte, so a fixture that
//! fires it would have to be a gibibyte — minutes of I/O per run for a fact the
//! store crate's own `should_reclaim` table already proves against the exact
//! numbers measured on the store that motivated this command (14 875 928 pages,
//! 9 880 851 of them free). What these tests cover is the wiring: the command
//! runs, reports, and — in `--dry-run` — leaves the file byte-for-byte alone.

#![cfg(unix)]

use std::process::{Output, Stdio};

use local_rag_core::paths::StoreLayout;
use local_rag_store::StateDb;
use local_rag_test_support::TempHome;

fn open_layout() -> (TempHome, StoreLayout) {
    let home = TempHome::new().expect("temp home");
    let layout = StoreLayout::new(home.join("local-rag"));
    layout.ensure().expect("ensure store tree");
    (home, layout)
}

fn run_cli(home: &TempHome, args: &[&str]) -> Output {
    let mut cmd = home.command(env!("CARGO_BIN_EXE_local-rag"));
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.output().expect("run local-rag")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Seed a store, then delete from it, so the file has holes to talk about.
fn seeded_store(layout: &StoreLayout) -> u64 {
    let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        db.writer()
            .transaction(|tx| {
                tx.execute_batch("CREATE TABLE x012 (blob BLOB);")?;
                for _ in 0..400 {
                    tx.execute("INSERT INTO x012 (blob) VALUES (randomblob(2000))", [])?;
                }
                Ok(())
            })
            .await
            .expect("seed");
        db.writer()
            .transaction(|tx| tx.execute_batch("DELETE FROM x012;"))
            .await
            .expect("delete");
        db.writer()
            .read_transaction(|tx| local_rag_store::db_space(tx))
            .await
            .expect("space")
            .page_count
    })
}

/// `X-012`: `--dry-run` reports and changes nothing.
///
/// The claim under test is the one an operator relies on before committing to
/// a rewrite that can run for many minutes: asking what it would do must cost
/// nothing but the answer.
#[test]
fn dry_run_reports_the_store_and_leaves_it_untouched() {
    let (home, layout) = open_layout();
    let before = seeded_store(&layout);

    let output = run_cli(&home, &["vacuum", "--dry-run"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("space:"), "{text}");
    assert!(text.contains("dry run, nothing changed"), "{text}");

    let db = StateDb::open(layout.state_db()).expect("reopen");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let after = rt
        .block_on(
            db.writer()
                .read_transaction(|tx| local_rag_store::db_space(tx)),
        )
        .expect("space")
        .page_count;
    assert_eq!(
        after, before,
        "a dry run must not move a single page: {before} -> {after}"
    );
}

/// `X-012`: a real run rewrites the file and the store comes back smaller.
///
/// Small fixture on purpose — this proves the wiring reaches SQLite and that
/// the command reports the difference it made, not how a 57 GB store behaves,
/// which only the live pass can answer.
#[test]
fn a_real_run_reclaims_and_says_how_much() {
    let (home, layout) = open_layout();
    let before = seeded_store(&layout);

    let output = run_cli(&home, &["vacuum"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("vacuum: reclaimed"), "{text}");

    let db = StateDb::open(layout.state_db()).expect("reopen");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let after = rt
        .block_on(
            db.writer()
                .read_transaction(|tx| local_rag_store::db_space(tx)),
        )
        .expect("space");
    assert!(
        after.page_count < before,
        "the rewrite must actually shrink the file: {before} -> {after:?}"
    );
    assert_eq!(
        after.auto_vacuum,
        local_rag_store::AutoVacuum::Incremental,
        "the rewrite is also the conversion — without it the daemon can never \
         reclaim at idle"
    );
}
