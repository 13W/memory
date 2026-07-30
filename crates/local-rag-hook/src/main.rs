//! `local-rag-hook` — spool writer invoked by Claude Code hooks.
//!
//! `version` is a diagnostic no-op; `spool-write` is the real hook write path
//! (spec 07 §2): parse hook JSON (stdin) → REDACTION
//! (`local_rag_hook::payload`, T13-01) → compute source identity (spec 07 §4)
//! → build the LRSP frame → durably append it (`local_rag_hook::segment`) →
//! for `SessionStart`/`UserPromptSubmit` only, a read-only recall RPC +
//! `additionalContext` print (`local_rag_hook::recall`, spec 11 §3.2/§5,
//! T15-06) → **always** exit 0 (fail-open, `[FIXED]`). Recall runs strictly
//! after the append has already durably succeeded, and not at all if it
//! failed — see `spool_write_pipeline`'s own call site.

use std::io::Read;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use local_rag_core::config::{Config, ConfigError};
use local_rag_core::paths::{PathError, StoreLayout, SystemEnv, config_dir};
use local_rag_core::redaction::Scanner;
use local_rag_core::spool::{FrameError, FramePayload};

use local_rag_hook::event::{self, EventPayload, ParseError};
use local_rag_hook::identity::{self, IdentityError};
use local_rag_hook::segment::{self, DEFAULT_ROTATE_THRESHOLD_BYTES, SpoolWriteError};
use local_rag_hook::subagent_counter::{self, CounterError};
use local_rag_hook::{clock, payload};

const BIN: &str = "local-rag-hook";
/// Self-imposed budget for the append path (spec 11 §3.1 `[SPEC]`). Not
/// enforced as a hard deadline — killing mid-write would risk an inconsistent
/// lock/file state — only measured and reported past the fact.
const APPEND_BUDGET: Duration = Duration::from_millis(200);

fn main() -> ExitCode {
    #[cfg(feature = "failpoints")]
    arm_failpoint_from_env();

    match std::env::args().nth(1).as_deref() {
        Some("version" | "--version" | "-V") => {
            println!("{}", local_rag_core::version_line(BIN));
            ExitCode::SUCCESS
        }
        Some("spool-write") => run_spool_write(),
        _ => {
            eprintln!("usage: {BIN} version|spool-write");
            ExitCode::from(2)
        }
    }
}

/// Self-arm a named failpoint from `LOCAL_RAG_HOOK_FAILPOINT` (spec 07 §7 S1/S2
/// kill tests). The hook is a separate OS process from its test harness, so a
/// parent test cannot reach across process boundaries to arm this process's
/// own failpoint registry directly — it sets this env var instead, and the
/// hook arms itself before doing anything else.
#[cfg(feature = "failpoints")]
fn arm_failpoint_from_env() {
    if let Ok(name) = std::env::var("LOCAL_RAG_HOOK_FAILPOINT") {
        let fp = local_rag_test_support::failpoint::global();
        fp.register(&name);
        let _ = fp.arm(&name, local_rag_test_support::Action::Abort);
    }
}

/// Every fallible step aggregated into one type, so a single `catch_unwind` +
/// match at the top can turn any of them into the same fail-open outcome.
#[derive(Debug)]
enum HookError {
    Stdin(std::io::Error),
    Parse(ParseError),
    Path(PathError),
    Config(ConfigError),
    Counter(CounterError),
    Identity(IdentityError),
    Frame(FrameError),
    Spool(SpoolWriteError),
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookError::Stdin(e) => write!(f, "reading stdin failed: {e}"),
            HookError::Parse(e) => write!(f, "{e}"),
            HookError::Path(e) => write!(f, "{e}"),
            HookError::Config(e) => write!(f, "{e}"),
            HookError::Counter(e) => write!(f, "{e}"),
            HookError::Identity(e) => write!(f, "{e}"),
            HookError::Frame(e) => write!(f, "{e}"),
            HookError::Spool(e) => write!(f, "{e}"),
        }
    }
}

impl From<ParseError> for HookError {
    fn from(e: ParseError) -> Self {
        HookError::Parse(e)
    }
}
impl From<PathError> for HookError {
    fn from(e: PathError) -> Self {
        HookError::Path(e)
    }
}
impl From<ConfigError> for HookError {
    fn from(e: ConfigError) -> Self {
        HookError::Config(e)
    }
}
impl From<CounterError> for HookError {
    fn from(e: CounterError) -> Self {
        HookError::Counter(e)
    }
}
impl From<IdentityError> for HookError {
    fn from(e: IdentityError) -> Self {
        HookError::Identity(e)
    }
}
impl From<FrameError> for HookError {
    fn from(e: FrameError) -> Self {
        HookError::Frame(e)
    }
}
impl From<SpoolWriteError> for HookError {
    fn from(e: SpoolWriteError) -> Self {
        HookError::Spool(e)
    }
}

/// Read stdin, run the write pipeline, and **always** exit 0 (spec 07 §2/11
/// §3.1 `[FIXED]`). `catch_unwind` is a safety net on top of the primary
/// discipline (every fallible step is a typed `Result`) — the workspace sets
/// no `panic = "abort"` profile override, so unwinding is real here, not a
/// no-op.
fn run_spool_write() -> ExitCode {
    let start = Instant::now();
    let mut raw = Vec::new();
    let read_result = std::io::stdin().read_to_end(&mut raw);

    // `io::Error` is not `UnwindSafe` (it can box an arbitrary `dyn Error`),
    // but nothing here relies on inspecting torn interior state after a
    // panic — we only ever turn a caught panic into "fail open, exit 0"
    // without touching whatever the panicking code partially mutated.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        read_result
            .map_err(HookError::Stdin)
            .and_then(|_| spool_write_pipeline(&raw))
    }));

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("{BIN}: fail-open: {e}"),
        Err(_) => eprintln!("{BIN}: fail-open: internal panic"),
    }

    let elapsed = start.elapsed();
    if elapsed > APPEND_BUDGET {
        eprintln!("{BIN}: append budget exceeded: {elapsed:?} (budget {APPEND_BUDGET:?})");
    }
    ExitCode::SUCCESS
}

/// The real hook write path (spec 07 §2), from raw stdin bytes to a durably
/// appended frame.
fn spool_write_pipeline(raw: &[u8]) -> Result<(), HookError> {
    let event = event::parse_hook_event(raw)?;

    let env = SystemEnv;
    let config = Config::load(&config_dir(&env)?)?;
    let layout = StoreLayout::resolve(&env)?;
    let session_dir = layout.spool_session(&event.session_id);

    let (tool_name, tool_input) = event::tool_context(&event.kind);
    let paths = event::extract_paths(tool_input);

    let stop_occurrence = match &event.kind {
        EventPayload::SubagentStop(p) => Some(subagent_counter::next_stop_occurrence(
            &session_dir,
            &p.agent_id,
        )?),
        _ => None,
    };

    let captured_at = clock::system_now_ms();
    let coarse = identity::coarse_ts(captured_at);
    let ident = identity::compute_identity(&event, coarse, stop_occurrence)?;
    let (evidence_kind, trust) = identity::evidence_kind_and_trust(&event.kind);

    let scanner = Scanner::new();
    let raw_text = String::from_utf8_lossy(raw);
    let prepared = payload::prepare_payload(&raw_text, &paths, tool_name, &config.spool, &scanner);

    let frame_payload = FramePayload {
        format_version: 1,
        source_event_id: ident.source_event_id,
        dedup_key: ident.dedup_key,
        event_type: event::event_type_name(&event.kind).to_string(),
        captured_at,
        session_id: event.session_id.clone(),
        agent_id: agent_id_of(&event.kind),
        turn_id: None,
        batch_id: None,
        worktree_root: event.cwd.clone(),
        commit: None,
        evidence_kind: evidence_kind.to_string(),
        trust: trust.to_string(),
        paths,
        redaction_version: payload::redaction_version_field(&prepared),
        payload: payload::payload_field(&prepared),
        short_evidence_excerpt: payload::short_evidence_excerpt_field(&prepared),
    };

    let frame_bytes = local_rag_core::spool::encode_frame(&frame_payload)?;
    segment::append_frame(
        &layout,
        &event.session_id,
        &frame_bytes,
        DEFAULT_ROTATE_THRESHOLD_BYTES,
    )?;

    // Read-only recall RPC (spec 11 §3.2), strictly after the append above
    // has already durably succeeded — never before, never at all if it
    // failed (the `?`s above would have returned first). Fail-open by
    // construction: `recall_and_print` never returns an error.
    if matches!(
        event.kind,
        EventPayload::SessionStart(_) | EventPayload::UserPromptSubmit(_)
    ) {
        local_rag_hook::recall::recall_and_print(&layout, &event);
    }

    Ok(())
}

/// The frame's `agent_id` field: populated only for `SubagentStop` (spec 07
/// §3's example shows it `null` for every other event type).
fn agent_id_of(kind: &EventPayload) -> Option<String> {
    match kind {
        EventPayload::SubagentStop(p) => Some(p.agent_id.clone()),
        _ => None,
    }
}
