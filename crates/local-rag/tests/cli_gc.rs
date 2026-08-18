//! `local-rag gc [--dry-run]` acceptance tests (spec 11 §6, D-025), driving
//! the real compiled binary against real fixture rows/directories for every
//! wired sweep — the original six, plus the generation retention sweep D-066
//! added (until then this command ran every sweep except the one spec 06 §5 is
//! actually about). Unlike the store crate's own deterministic-clock unit
//! tests for each sweep (`crates/store/tests/housekeeping.rs`,
//! `src/observation/payload_ttl.rs`), the CLI binary reads the *real* wall
//! clock (`cli/mod.rs::system_now_ms`) — so every time-gated fixture here is
//! seeded comfortably past its budget relative to the real clock rather than
//! against a fake one. This file's job is to prove the wiring (all six
//! sweeps run, dry-run changes nothing, a real run removes/expires, a
//! second real run is a no-op), not to re-prove each sweep's own business
//! logic, which is already exhaustively covered where the sweep lives.

#![cfg(unix)]

use std::fs;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use local_rag_core::identity::{Uuid, UuidSource, uuidv7_from};
use local_rag_core::paths::StoreLayout;
use local_rag_core::spool::{FramePayload, encode_frame, encode_segment_header};
use local_rag_store::registry::{GenerationState, transition_generation};
use local_rag_store::{
    CANDIDATE_EXPIRY_MS, NewCandidate, RequestRoot, SHARD_DESTROY_GRACE_MS,
    SPOOL_SESSION_ABSENCE_MS, StateDb, WorktreeKind, WorktreeState, allocate_generation,
    create_candidate, create_repository, create_worktree, import_session_tail,
    insert_projection_state, transition_worktree_state,
};
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

/// The real wall clock, milliseconds since the epoch — the CLI binary
/// itself has no other clock to read `system_now_ms()` from, so every
/// time-gated fixture below is seeded relative to this, not a fake tick.
fn real_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64
}

/// One hour of headroom past each budget, so a few seconds of real wall-clock
/// drift between seeding and the CLI subprocess actually running can never
/// flip a fixture from "due" to "not yet due".
const HEADROOM_MS: i64 = 60 * 60 * 1_000;

/// The default `[storage].retired_generations_ttl_h` (168 h) in milliseconds —
/// the CLI reads the real config, so a generation must be seeded older than this
/// to fall outside the retention window (D-066).
const RETENTION_WINDOW_MS: i64 = 168 * 60 * 60 * 1_000;

fn uuid(seed: u8) -> String {
    let mut rand = [0u8; 10];
    rand[9] = seed;
    uuidv7_from(1000, rand).to_string()
}

struct SeqUuidV7 {
    counter: AtomicU64,
}
impl SeqUuidV7 {
    fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }
}
impl UuidSource for SeqUuidV7 {
    fn next_uuid(&self) -> Uuid {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        uuidv7_from(1000 + n, [0xAB; 10])
    }
}

fn make_shard(layout: &StoreLayout, name: &str) {
    let dir = layout.projection_shard(name);
    fs::create_dir_all(&dir).expect("mkdir shard");
    fs::write(dir.join("segment.bin"), b"x").expect("write shard file");
}

fn make_space_shard(layout: &StoreLayout, wt: &str, space: &str) {
    let dir = layout.projection_shard_space(wt, space);
    fs::create_dir_all(&dir).expect("mkdir space shard");
    fs::write(dir.join("segment.bin"), b"x").expect("write shard file");
}

async fn insert_model_space(db: &StateDb, id: &str, state: &str) {
    let (i, s) = (id.to_string(), state.to_string());
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO model_space (model_space_id, display_name, state, created_at, \
                 updated_at) VALUES (?1, ?2, ?3, 1000, 1000)",
                rusqlite::params![i, format!("space-{i}"), s],
            )
            .map(|_| ())
        })
        .await
        .expect("insert model space");
}

async fn project_onto(db: &StateDb, worktree_id: &str, generation_id: &str, model_space_id: &str) {
    let w = worktree_id.to_string();
    db.writer()
        .transaction(move |tx| insert_projection_state(tx, &w, 1000))
        .await
        .expect("init projection state");
    let (w, g, m) = (
        worktree_id.to_string(),
        generation_id.to_string(),
        model_space_id.to_string(),
    );
    db.writer()
        .transaction(move |tx| {
            tx.execute(
                "UPDATE worktree_projection_state \
                 SET active_generation_id = ?2, active_model_space_id = ?3, \
                     projected_generation_id = ?2, projected_model_space_id = ?3 \
                 WHERE worktree_id = ?1",
                rusqlite::params![w, g, m],
            )
            .map(|_| ())
        })
        .await
        .expect("point projection state");
}

fn spool_fixture(session_id: &str, source_event_id: &str, captured_at: i64) -> FramePayload {
    FramePayload {
        format_version: 1,
        source_event_id: source_event_id.to_string(),
        dedup_key: None,
        event_type: "Stop".to_string(),
        captured_at,
        session_id: session_id.to_string(),
        agent_id: None,
        turn_id: None,
        batch_id: None,
        worktree_root: None,
        commit: None,
        evidence_kind: "model_claim".to_string(),
        trust: "low".to_string(),
        paths: vec![],
        redaction_version: Some(1),
        payload: Some("{}".to_string()),
        short_evidence_excerpt: None,
    }
}

fn write_spool_segment(layout: &StoreLayout, session_id: &str, seq: u32, frames: &[FramePayload]) {
    let session_dir = layout.spool_session(session_id);
    fs::create_dir_all(&session_dir).expect("session dir");
    let mut bytes = encode_segment_header().to_vec();
    for f in frames {
        bytes.extend_from_slice(&encode_frame(f).expect("under the frame cap"));
    }
    fs::write(session_dir.join(format!("{seq:06}.seg")), bytes).expect("write segment");
}

/// Seed one instance of each of the six sweep-eligible conditions `gc` wires
/// together, all relative to the real wall clock: an orphan shard dir, an
/// expired detached-worktree shard, an unreferenced model-space shard, a
/// fully-committed absent spool session (whose payload — 72h TTL, imported
/// far in the past — is also already past its own TTL, covering that sixth
/// condition without a second session), and a 30-day-stale pending
/// candidate.
async fn seed_all_six(db: &StateDb, layout: &StoreLayout) {
    let repo = {
        let r = uuid(1);
        let repo = r.clone();
        db.writer()
            .transaction(move |tx| create_repository(tx, &repo, None, 1000))
            .await
            .expect("create repository");
        r
    };

    // 0. D-066's generation sweep: three `retiring` generations on their own
    //    worktree, all created well past the default 168h retention window, so
    //    the window pins none of them and `keep_last_k = 2` pins exactly the two
    //    highest-numbered — leaving the oldest as the single sweep candidate.
    //    Deliberately more than one: "removed 1" out of three proves the
    //    retention policy is being applied, not that the sweep just took
    //    whatever it found.
    let wt_retired = uuid(12);
    {
        let (r, w) = (repo.clone(), wt_retired.clone());
        db.writer()
            .transaction(move |tx| create_worktree(tx, &w, &r, WorktreeKind::Main, 1000))
            .await
            .expect("create worktree");
    }
    let stale_at = real_now_ms() - RETENTION_WINDOW_MS - HEADROOM_MS;
    for seed in [70u8, 71, 72] {
        let (w, g) = (wt_retired.clone(), uuid(seed));
        db.writer()
            .transaction(move |tx| {
                allocate_generation(tx, &w, &g, stale_at)?;
                for to in [
                    GenerationState::ProjectionReady,
                    GenerationState::Active,
                    GenerationState::Retiring,
                ] {
                    transition_generation(tx, &g, to)?.expect("legal transition");
                }
                Ok(())
            })
            .await
            .expect("seed retiring generation");
    }

    // 1. Orphan shard dir (no worktree row at all).
    make_shard(layout, "orphan-shard");

    // 2. Expired detached-worktree shard (D-007): well past the 7-day grace.
    let wt_detached = uuid(10);
    {
        let (r, w) = (repo.clone(), wt_detached.clone());
        db.writer()
            .transaction(move |tx| create_worktree(tx, &w, &r, WorktreeKind::Main, 1000))
            .await
            .expect("create worktree");
        let changed_at = real_now_ms() - SHARD_DESTROY_GRACE_MS - HEADROOM_MS;
        let w = wt_detached.clone();
        db.writer()
            .transaction(move |tx| {
                transition_worktree_state(tx, &w, WorktreeState::Detached, changed_at)
            })
            .await
            .expect("transition tx")
            .expect("legal transition");
    }
    make_shard(layout, &wt_detached);

    // 3. Unreferenced model-space shard (D-011): worktree active on space B,
    //    stray directory left over for space A.
    let wt_active = uuid(11);
    {
        let (r, w) = (repo.clone(), wt_active.clone());
        db.writer()
            .transaction(move |tx| create_worktree(tx, &w, &r, WorktreeKind::Main, 1000))
            .await
            .expect("create worktree");
    }
    let space_a = uuid(50);
    let space_b = uuid(51);
    insert_model_space(db, &space_a, "retiring").await;
    insert_model_space(db, &space_b, "active").await;
    let generation = {
        let genr = uuid(60);
        let (w, g) = (wt_active.clone(), genr.clone());
        db.writer()
            .transaction(move |tx| allocate_generation(tx, &w, &g, 1000).map(|_| ()))
            .await
            .expect("allocate generation");
        genr
    };
    project_onto(db, &wt_active, &generation, &space_b).await;
    make_space_shard(layout, &wt_active, &space_a);
    make_space_shard(layout, &wt_active, &space_b);

    // 4 & 6. Dead spool session (T13-05) whose payload is also past its own
    // TTL (spec 12 §3): imported comfortably past the 14-day absence budget,
    // with the default 72h payload TTL — 14 days is far longer than 72h, so
    // both conditions are already true by construction.
    let session = "sess-old";
    let import_now = real_now_ms() - SPOOL_SESSION_ABSENCE_MS - HEADROOM_MS;
    write_spool_segment(
        layout,
        session,
        1,
        &[spool_fixture(session, "st:sess-old:1", import_now)],
    );
    let uuids = SeqUuidV7::new();
    import_session_tail(
        db,
        layout,
        session,
        &RequestRoot::default(),
        &uuids,
        import_now,
        72,
    )
    .await
    .expect("import spool session");

    // 5. Stale pending candidate (T14-05): well past the 30-day expiry
    // budget.
    let created_at = real_now_ms() - CANDIDATE_EXPIRY_MS - HEADROOM_MS;
    db.writer()
        .transaction(move |tx| {
            create_candidate(
                tx,
                &NewCandidate {
                    candidate_id: "cand-stale",
                    proposed_operation: "{\"op\":\"resolve\",\"memory_id\":\"m\",\"expected_version\":1}",
                    conflicts: None,
                },
                created_at,
            )
        })
        .await
        .expect("seed pending candidate");
}

#[test]
fn gc_rejects_an_unknown_argument() {
    let (home, _layout) = open_layout();
    let output = run_cli(&home, &["gc", "--bogus"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[tokio::test]
async fn dry_run_reports_all_six_conditions_and_changes_nothing() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_all_six(&state, &layout).await;
    }

    let output = run_cli(&home, &["gc", "--dry-run"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(
        text.contains("generations: would remove 1"),
        "the generation sweep (D-066) must be reported first: {text}"
    );
    assert!(text.contains("orphan shard dirs: would remove 1"), "{text}");
    assert!(
        text.contains("expired shard dirs: would remove 1"),
        "{text}"
    );
    assert!(
        text.contains("unreferenced model-space dirs: would remove 1"),
        "{text}"
    );
    assert!(
        text.contains("dead spool sessions: would remove 1"),
        "{text}"
    );
    assert!(
        text.contains("expired observation payloads: would remove 1"),
        "{text}"
    );
    assert!(
        text.contains("stale pending candidates: would expire 1"),
        "{text}"
    );

    // Nothing was actually touched.
    assert!(layout.projection_shard("orphan-shard").is_dir());
    assert!(layout.spool_session("sess-old").is_dir());
}

#[tokio::test]
async fn a_real_run_removes_all_six_and_a_second_run_is_a_no_op() {
    let (home, layout) = open_layout();
    {
        let state = StateDb::open(layout.state_db()).expect("open state.sqlite");
        seed_all_six(&state, &layout).await;
    }

    let output = run_cli(&home, &["gc"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("generations: removed 1"), "{text}");
    assert!(text.contains("orphan shard dirs: removed 1"), "{text}");
    assert!(text.contains("expired shard dirs: removed 1"), "{text}");
    assert!(
        text.contains("unreferenced model-space dirs: removed 1"),
        "{text}"
    );
    assert!(text.contains("dead spool sessions: removed 1"), "{text}");
    assert!(
        text.contains("expired observation payloads: removed 1"),
        "{text}"
    );
    assert!(
        text.contains("stale pending candidates: expired 1"),
        "{text}"
    );

    assert!(!layout.projection_shard("orphan-shard").exists());
    assert!(!layout.spool_session("sess-old").exists());

    // Idempotent: nothing left to do on a second real run.
    let second = run_cli(&home, &["gc"]);
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    let text = stdout(&second);
    assert!(
        text.contains("generations: removed 0"),
        "a second sweep has nothing left to collect: {text}"
    );
    assert!(text.contains("orphan shard dirs: removed 0"), "{text}");
    assert!(text.contains("expired shard dirs: removed 0"), "{text}");
    assert!(
        text.contains("unreferenced model-space dirs: removed 0"),
        "{text}"
    );
    assert!(text.contains("dead spool sessions: removed 0"), "{text}");
    assert!(
        text.contains("stale pending candidates: expired 0"),
        "{text}"
    );
}
