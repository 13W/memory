//! `T23-07` / ADR-0014 Decision 2 / `D-118`: the card's own "distinct-vs-total
//! figure remeasured" acceptance, against the owner's real backlog.
//!
//! Both tests are `#[ignore]`d and read `LOCAL_RAG_LIVE_ROOT` — a store root
//! (the directory holding `state.sqlite`), opened **read only, through a
//! URI**, never through `StateDb` (which would migrate a live store under a
//! running daemon). Nothing here writes, and nothing here needs a model:
//! this measures `pending_memory_candidate` directly, the same table
//! `local_rag_store::memory::review::propose_candidate`'s own dedup check
//! reads, using the same exported [`candidate_dedup_key`] that check uses —
//! so grouping by it here is a fair proxy for what the check would decide,
//! without needing its private SQL helper.
//!
//! Reproduce:
//!
//! ```text
//! LOCAL_RAG_LIVE_ROOT=~/.local/share/local-rag \
//!   cargo test -p local-rag --test candidate_dedup_live -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use local_rag_core::paths::StoreLayout;
use local_rag_store::memory::{CandidateDedupKey, ProposedOperation, candidate_dedup_key};
use rusqlite::{Connection, OpenFlags};

fn live_root() -> Option<PathBuf> {
    std::env::var_os("LOCAL_RAG_LIVE_ROOT").map(PathBuf::from)
}

fn open_live_read_only(root: &std::path::Path) -> Connection {
    let layout = StoreLayout::new(root.to_path_buf());
    let uri = format!("file:{}?mode=ro", layout.state_db().display());
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open the live store read-only")
}

/// Every still-`pending` `create`-shaped proposal's `candidate_id` grouped by
/// [`candidate_dedup_key`] — the same identity `propose_candidate`'s own
/// check groups on, computed the same way, just read back rather than
/// checked at write time.
fn pending_create_groups(conn: &Connection) -> HashMap<CandidateDedupKey, Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT candidate_id, proposed_operation FROM pending_memory_candidate \
             WHERE review_state = 'pending' \
               AND json_extract(proposed_operation, '$.op') = 'create'",
        )
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("query");

    let mut groups: HashMap<CandidateDedupKey, Vec<String>> = HashMap::new();
    for row in rows {
        let (candidate_id, proposed_json) = row.expect("row");
        let Ok(op) = serde_json::from_str::<ProposedOperation>(&proposed_json) else {
            continue;
        };
        groups
            .entry(candidate_dedup_key(&op))
            .or_default()
            .push(candidate_id);
    }
    groups
}

/// The card's own evidence figure: total pending `create` proposals against
/// the distinct claims among them (ADR-0014: "the count of distinct texts is
/// the invariant a later, fuzzier step would have to beat" — `T23-08`'s own
/// acceptance).
#[test]
#[ignore = "needs LOCAL_RAG_LIVE_ROOT"]
fn measure_distinct_versus_total_pending_claims() {
    let Some(root) = live_root() else {
        eprintln!("skipped: LOCAL_RAG_LIVE_ROOT not set");
        return;
    };
    let conn = open_live_read_only(&root);
    let groups = pending_create_groups(&conn);

    let total: usize = groups.values().map(Vec::len).sum();
    let distinct = groups.len();
    let worst = groups.values().map(Vec::len).max().unwrap_or(0);

    println!("pending create proposals: {total}");
    println!("distinct claims (candidate_dedup_key): {distinct}");
    println!("worst duplicated claim: {worst} copies");

    assert!(
        total > 0,
        "the store produced no pending candidates to measure"
    );
    assert!(
        distinct <= total,
        "grouping cannot invent claims that were not there"
    );
}

/// The card's acceptance, offline and without a write: the text this store
/// duplicated the most is exactly the shape `propose_candidate`'s check now
/// declines a second row of.
#[test]
#[ignore = "needs LOCAL_RAG_LIVE_ROOT"]
fn the_worst_duplicated_claim_would_now_be_dropped() {
    let Some(root) = live_root() else {
        eprintln!("skipped: LOCAL_RAG_LIVE_ROOT not set");
        return;
    };
    let conn = open_live_read_only(&root);
    let groups = pending_create_groups(&conn);

    let worst = groups
        .iter()
        .max_by_key(|(_, ids)| ids.len())
        .expect("the store produced at least one pending create");

    println!(
        "worst duplicated claim: {} copies, e.g. candidate_id {}",
        worst.1.len(),
        worst.1[0]
    );
    assert!(
        worst.1.len() > 1,
        "expected at least one claim proposed more than once on this store: {worst:?}"
    );
}
