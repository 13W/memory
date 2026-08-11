//! Code-level enforcement of spec 08 §4's two `[FIXED]` router placement
//! rules (T14-07). The model's own claims are never trusted as the
//! enforcement mechanism — only as a *signal* this module independently
//! verifies against the window's own [`WindowObservation::evidence_kind`]
//! fields (set at write time by the hook/import layer, T13-04) and a fresh
//! database read. [`materialize`] is the module's single entry point:
//! [`crate::schema::RawRouterOp`] in, [`local_rag_store::GeneratedOp`] out —
//! never fallible in the [`crate::parse`] sense, because every failure mode
//! here is a *per-op* degradation (to `noop` or `propose_candidate`), never a
//! whole-response rejection (see [`crate::parse`]'s module doc for why that
//! split is drawn where it is).
//!
//! # The two rules
//!
//! - **"Auto-save only for explicit durable decisions/instructions."** A
//!   `create`/`supersede` minting a new entry of `kind ∈ {fact, decision,
//!   convention, procedure, task}` needs at least one cited observation that
//!   is both **inside this window** and has `evidence_kind ==
//!   user_statement`. Missing → [`local_rag_store::GeneratedOp::ProposeCandidate`].
//!   `question`/`hypothesis` are exempt — creating one of those *is* the
//!   "not durable yet" outcome the rule is steering toward, not something it
//!   needs to gate.
//! - **"Model-claims are never auto-promoted to facts."** The same `create`/
//!   `supersede`, narrowed to `kind ∈ {fact, decision, convention,
//!   procedure}` (`task` excluded — mirrors
//!   [`local_rag_store::memory::op`]'s own `is_promotion_kind`, the
//!   store-level backstop this rule proactively avoids ever tripping):
//!   evidence resolved from **anywhere** (window or an older observation via
//!   [`local_rag_store::observation_evidence_source`]) that is non-empty and
//!   entirely `model_claim` → also [`local_rag_store::GeneratedOp::ProposeCandidate`].
//!   When the first rule already passed for one of the two rules'
//!   overlapping kinds, this one cannot additionally fire (a `user_statement`
//!   citation is itself non-`model_claim`) — the two checks still run
//!   independently, so a `Create` reached via the durable-exempt kinds
//!   (`question`/`hypothesis`) is *not* accidentally also exempted from this
//!   rule (it isn't a promotion kind either, so it never applies there, but
//!   for the right reason, not by accident).
//!
//! # Never forward a reference the runner cannot resolve
//!
//! [`local_rag_store::memory::runner::apply_run`]'s own `resolve_evidence`
//! hard-errors — aborting the *whole* consolidation batch, per that module's
//! atomicity guarantee — on any `observation_id` it cannot resolve. Since the
//! runner always re-invokes the (deterministic, greedy-decoded) generator
//! from scratch on retry, forwarding a citation this module could not
//! resolve either would reproduce the identical rejection forever: a
//! livelock. [`resolve_citations`] therefore silently drops any citation
//! that resolves nowhere rather than forwarding it — the same reasoning
//! [`canonical_key_owner`] pre-checks below apply to a `create`/`supersede`
//! whose `canonical_key` would collide with an existing row (the exact
//! scenario [`local_rag_store::memory::op`]'s `create_new_entry` would
//! otherwise reject at commit time).
//!
//! # Targets are always re-resolved fresh
//!
//! Every `target_memory_id` is resolved via [`crate::recall::resolve_target`]
//! against the read connection passed to [`materialize`] — never a version
//! the model echoed. See that function's module doc for why.

use std::collections::HashMap;

use local_rag_core::identity::UuidSource;
use local_rag_store::rusqlite;
use local_rag_store::rusqlite::Connection;
use local_rag_store::{
    EvidenceKind, MemoryKind, MemoryState, ProposedOperation, ScopeKind, WindowObservation,
    canonical_key_owner, observation_evidence_source,
};

use crate::recall::resolve_target;
use crate::schema::{RawRouterOp, Signal};

/// One resolved citation: which [`EvidenceKind`] it carries, and whether it
/// was found inside the current window (only an in-window citation counts
/// toward the "explicit durable" gate — see the module doc) or resolved
/// externally.
struct ResolvedCitation {
    observation_id: String,
    evidence_kind: EvidenceKind,
    in_window: bool,
}

fn resolve_citations(
    conn: &Connection,
    by_id: &HashMap<&str, &WindowObservation>,
    cites: &[String],
) -> rusqlite::Result<Vec<ResolvedCitation>> {
    let mut out = Vec::with_capacity(cites.len());
    for id in cites {
        if let Some(w) = by_id.get(id.as_str()) {
            out.push(ResolvedCitation {
                observation_id: id.clone(),
                evidence_kind: w.evidence_kind,
                in_window: true,
            });
            continue;
        }
        if let Some((evidence_kind, _session_id)) = observation_evidence_source(conn, id)? {
            out.push(ResolvedCitation {
                observation_id: id.clone(),
                evidence_kind,
                in_window: false,
            });
        }
        // else: a hallucinated reference -- dropped, never forwarded (module doc).
    }
    Ok(out)
}

fn cited_ids(cited: &[ResolvedCitation]) -> Vec<String> {
    cited.iter().map(|c| c.observation_id.clone()).collect()
}

fn is_durable_kind(kind: MemoryKind) -> bool {
    matches!(
        kind,
        MemoryKind::Fact
            | MemoryKind::Decision
            | MemoryKind::Convention
            | MemoryKind::Procedure
            | MemoryKind::Task
    )
}

fn is_promotion_kind(kind: MemoryKind) -> bool {
    matches!(
        kind,
        MemoryKind::Fact | MemoryKind::Decision | MemoryKind::Convention | MemoryKind::Procedure
    )
}

fn fails_explicit_durable_gate(kind: MemoryKind, cited: &[ResolvedCitation]) -> bool {
    is_durable_kind(kind)
        && !cited
            .iter()
            .any(|c| c.in_window && c.evidence_kind == EvidenceKind::UserStatement)
}

fn fails_model_claim_only_gate(kind: MemoryKind, cited: &[ResolvedCitation]) -> bool {
    is_promotion_kind(kind)
        && !cited.is_empty()
        && cited
            .iter()
            .all(|c| c.evidence_kind == EvidenceKind::ModelClaim)
}

fn needs_placement_review(kind: MemoryKind, cited: &[ResolvedCitation]) -> bool {
    fails_explicit_durable_gate(kind, cited) || fails_model_claim_only_gate(kind, cited)
}

/// Resolve a new entry's `scope_owner_id` (T14-07: `WindowObservation` has no
/// direct scope-target field, only the `repo_id`/`worktree_id` each
/// observation itself was captured against). `global` always resolves to the
/// fixed singleton. `repository`/`worktree` prefer a cited observation's own
/// id, falling back to the first non-null value anywhere in the window (a
/// pragmatic as-built choice for a window that happens to span more than one
/// repo/worktree — the common case of a single-repo session makes this
/// ambiguity rare in practice). `None` when the window carries no such id at
/// all — the caller cannot place the entry anywhere and must not guess.
fn resolve_scope_owner(
    scope_kind: ScopeKind,
    cited: &[ResolvedCitation],
    by_id: &HashMap<&str, &WindowObservation>,
    window: &[WindowObservation],
) -> Option<String> {
    match scope_kind {
        ScopeKind::Global => Some(local_rag_store::GLOBAL_SCOPE_OWNER_ID.to_string()),
        ScopeKind::Repository => cited
            .iter()
            .filter_map(|c| by_id.get(c.observation_id.as_str()))
            .find_map(|w| w.repo_id.clone())
            .or_else(|| window.iter().find_map(|w| w.repo_id.clone())),
        ScopeKind::Worktree => cited
            .iter()
            .filter_map(|c| by_id.get(c.observation_id.as_str()))
            .find_map(|w| w.worktree_id.clone())
            .or_else(|| window.iter().find_map(|w| w.worktree_id.clone())),
    }
}

/// Handles both [`RawRouterOp::Create`] (`force_candidate = false`, subject
/// to both placement gates) and [`RawRouterOp::ProposeCandidate`]
/// (`force_candidate = true` — the model's own request, which always wins
/// over what the gates would have decided anyway, since `propose_candidate`
/// is already the strictest legal outcome).
#[allow(clippy::too_many_arguments)]
fn handle_create(
    conn: &Connection,
    by_id: &HashMap<&str, &WindowObservation>,
    window: &[WindowObservation],
    uuids: &dyn UuidSource,
    force_candidate: bool,
    kind: String,
    text: String,
    canonical_key: Option<String>,
    scope_kind: String,
    confidence_signal: Option<Signal>,
    importance_signal: Option<Signal>,
    cites: Vec<String>,
) -> rusqlite::Result<local_rag_store::GeneratedOp> {
    use local_rag_store::GeneratedOp;

    let Some(kind) = MemoryKind::from_db(&kind) else {
        return Ok(GeneratedOp::Noop);
    };
    let Some(scope_kind) = ScopeKind::from_db(&scope_kind) else {
        return Ok(GeneratedOp::Noop);
    };
    // D-051: a missing signal is per-op information loss, not a value to
    // invent (spec 08 §2's "never invent a numeric confidence") — degrade
    // this one op to `Noop`, the same tier-2 treatment `kind`/`scope_kind`
    // already get above, rather than fabricating `Signal::Low`.
    let Some(confidence_signal) = confidence_signal else {
        return Ok(GeneratedOp::Noop);
    };
    let Some(importance_signal) = importance_signal else {
        return Ok(GeneratedOp::Noop);
    };
    let cited = resolve_citations(conn, by_id, &cites)?;
    let evidence_observation_ids = cited_ids(&cited);
    let Some(scope_owner_id) = resolve_scope_owner(scope_kind, &cited, by_id, window) else {
        return Ok(GeneratedOp::Noop);
    };
    let canonical_conflict = match &canonical_key {
        Some(key) => canonical_key_owner(conn, scope_kind, &scope_owner_id, key)?,
        None => None,
    };
    let needs_review = force_candidate || needs_placement_review(kind, &cited);

    let operation = ProposedOperation::Create {
        memory_id: uuids.next_uuid().to_string(),
        kind: kind.as_str().to_string(),
        text,
        canonical_key,
        scope_kind: scope_kind.as_str().to_string(),
        scope_owner_id,
        confidence: confidence_signal.confidence(),
        importance: importance_signal.importance(),
        valid_from_tree: None,
        last_verified_tree: None,
    };

    if needs_review || canonical_conflict.is_some() {
        return Ok(GeneratedOp::ProposeCandidate {
            candidate_id: uuids.next_uuid().to_string(),
            operation,
            conflicts: canonical_conflict.into_iter().collect(),
            evidence_observation_ids,
        });
    }
    Ok(GeneratedOp::Materialize {
        operation,
        evidence_observation_ids,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_supersede(
    conn: &Connection,
    by_id: &HashMap<&str, &WindowObservation>,
    uuids: &dyn UuidSource,
    target_memory_id: String,
    new_kind: String,
    new_text: String,
    new_canonical_key: Option<String>,
    confidence_signal: Option<Signal>,
    importance_signal: Option<Signal>,
    cites: Vec<String>,
) -> rusqlite::Result<local_rag_store::GeneratedOp> {
    use local_rag_store::GeneratedOp;

    let Some(old) = resolve_target(conn, &target_memory_id)? else {
        return Ok(GeneratedOp::Noop);
    };
    let Some(new_kind) = MemoryKind::from_db(&new_kind) else {
        return Ok(GeneratedOp::Noop);
    };
    if old
        .state
        .check_transition(old.kind, MemoryState::Superseded)
        .is_err()
    {
        // e.g. a task/question: that kind's machine never allows supersede.
        return Ok(GeneratedOp::Noop);
    }
    // D-051: see the identical check in `handle_create` above.
    let Some(confidence_signal) = confidence_signal else {
        return Ok(GeneratedOp::Noop);
    };
    let Some(importance_signal) = importance_signal else {
        return Ok(GeneratedOp::Noop);
    };

    let cited = resolve_citations(conn, by_id, &cites)?;
    let evidence_observation_ids = cited_ids(&cited);

    // Supersession inherits the old entry's own scope (schema.rs's module
    // doc: the wire schema deliberately carries no `scope_kind` for
    // `supersede`). A `new_canonical_key` equal to the old entry's own key
    // is dropped rather than reused: `create_new_entry`'s conflict check has
    // no state filter, and the old row still physically holds that key at
    // insert time (new-then-retire ordering, `local_rag_store::memory::op`'s
    // own module doc) -- reusing it would deterministically conflict on
    // every retry.
    let new_canonical_key = match new_canonical_key {
        Some(key) if Some(key.as_str()) == old.canonical_key.as_deref() => None,
        other => other,
    };
    let canonical_conflict = match &new_canonical_key {
        Some(key) => canonical_key_owner(conn, old.scope_kind, &old.scope_owner_id, key)?
            .filter(|owner| owner != &old.memory_id),
        None => None,
    };

    let needs_review = needs_placement_review(new_kind, &cited);

    let operation = ProposedOperation::Supersede {
        old_memory_id: old.memory_id.clone(),
        old_expected_version: old.entry_version,
        new_memory_id: uuids.next_uuid().to_string(),
        new_kind: new_kind.as_str().to_string(),
        new_text,
        new_canonical_key,
        new_scope_kind: old.scope_kind.as_str().to_string(),
        new_scope_owner_id: old.scope_owner_id,
        new_confidence: confidence_signal.confidence(),
        new_importance: importance_signal.importance(),
        new_valid_from_tree: None,
        new_last_verified_tree: None,
    };

    if needs_review || canonical_conflict.is_some() {
        return Ok(GeneratedOp::ProposeCandidate {
            candidate_id: uuids.next_uuid().to_string(),
            operation,
            conflicts: canonical_conflict.into_iter().collect(),
            evidence_observation_ids,
        });
    }
    Ok(GeneratedOp::Materialize {
        operation,
        evidence_observation_ids,
    })
}

fn handle_reinforce(
    conn: &Connection,
    by_id: &HashMap<&str, &WindowObservation>,
    target_memory_id: String,
    confidence_signal: Option<Signal>,
    cites: Vec<String>,
) -> rusqlite::Result<local_rag_store::GeneratedOp> {
    use local_rag_store::GeneratedOp;

    let Some(target) = resolve_target(conn, &target_memory_id)? else {
        return Ok(GeneratedOp::Noop);
    };
    let cited = resolve_citations(conn, by_id, &cites)?;
    let evidence_observation_ids = cited_ids(&cited);
    let operation = ProposedOperation::Reinforce {
        memory_id: target.memory_id,
        expected_version: target.entry_version,
        confidence: confidence_signal.map(Signal::confidence),
    };
    Ok(GeneratedOp::Materialize {
        operation,
        evidence_observation_ids,
    })
}

/// Shared by `resolve`/`retract` (spec 04 §5): both are plain state
/// transitions with no placement gate, but the kind-specific machine may
/// still forbid the transition (e.g. `resolve` on a `fact`) -- pre-checked
/// here for the same livelock reason [`handle_supersede`]'s own check
/// exists.
fn handle_terminal_transition(
    conn: &Connection,
    by_id: &HashMap<&str, &WindowObservation>,
    target_memory_id: String,
    cites: Vec<String>,
    to: MemoryState,
    build: impl FnOnce(String, i64) -> ProposedOperation,
) -> rusqlite::Result<local_rag_store::GeneratedOp> {
    use local_rag_store::GeneratedOp;

    let Some(target) = resolve_target(conn, &target_memory_id)? else {
        return Ok(GeneratedOp::Noop);
    };
    if target.state.check_transition(target.kind, to).is_err() {
        return Ok(GeneratedOp::Noop);
    }
    let cited = resolve_citations(conn, by_id, &cites)?;
    let evidence_observation_ids = cited_ids(&cited);
    let operation = build(target.memory_id, target.entry_version);
    Ok(GeneratedOp::Materialize {
        operation,
        evidence_observation_ids,
    })
}

/// Turn one [`RawRouterOp`] into a [`local_rag_store::GeneratedOp`] (see the
/// module doc). `by_id` is a lookup over the same window `conn` was loaded
/// for; `window` is that window's observation slice (for the scope-owner
/// fallback). Never fails at the Rust-error level for a semantic reason —
/// every degradation is expressed as the returned `GeneratedOp` itself
/// (`Noop`/`ProposeCandidate`); the `rusqlite::Result` wrapper is only for
/// genuine I/O failure on `conn`.
pub fn materialize(
    conn: &Connection,
    by_id: &HashMap<&str, &WindowObservation>,
    window: &[WindowObservation],
    uuids: &dyn UuidSource,
    raw: RawRouterOp,
) -> rusqlite::Result<local_rag_store::GeneratedOp> {
    use local_rag_store::GeneratedOp;

    match raw {
        RawRouterOp::Create {
            kind,
            text,
            canonical_key,
            scope_kind,
            confidence_signal,
            importance_signal,
            cites,
        } => handle_create(
            conn,
            by_id,
            window,
            uuids,
            false,
            kind,
            text,
            canonical_key,
            scope_kind,
            confidence_signal,
            importance_signal,
            cites,
        ),
        RawRouterOp::ProposeCandidate {
            kind,
            text,
            canonical_key,
            scope_kind,
            confidence_signal,
            importance_signal,
            cites,
        } => handle_create(
            conn,
            by_id,
            window,
            uuids,
            true,
            kind,
            text,
            canonical_key,
            scope_kind,
            confidence_signal,
            importance_signal,
            cites,
        ),
        RawRouterOp::Reinforce {
            target_memory_id,
            confidence_signal,
            cites,
        } => handle_reinforce(conn, by_id, target_memory_id, confidence_signal, cites),
        RawRouterOp::Resolve {
            target_memory_id,
            cites,
        } => handle_terminal_transition(
            conn,
            by_id,
            target_memory_id,
            cites,
            MemoryState::Resolved,
            |memory_id, expected_version| ProposedOperation::Resolve {
                memory_id,
                expected_version,
            },
        ),
        RawRouterOp::Retract {
            target_memory_id,
            cites,
        } => handle_terminal_transition(
            conn,
            by_id,
            target_memory_id,
            cites,
            MemoryState::Retracted,
            |memory_id, expected_version| ProposedOperation::Retract {
                memory_id,
                expected_version,
            },
        ),
        RawRouterOp::Supersede {
            target_memory_id,
            new_kind,
            new_text,
            new_canonical_key,
            confidence_signal,
            importance_signal,
            cites,
        } => handle_supersede(
            conn,
            by_id,
            uuids,
            target_memory_id,
            new_kind,
            new_text,
            new_canonical_key,
            confidence_signal,
            importance_signal,
            cites,
        ),
        RawRouterOp::Noop { .. } => Ok(GeneratedOp::Noop),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use local_rag_core::identity::{Uuid, uuidv7_from};
    use local_rag_core::paths::StoreLayout;
    use local_rag_store::rusqlite::params;
    use local_rag_store::{
        GeneratedOp, MemoryKind as StoreMemoryKind, NewMemoryEntry, StateDb, TrustLevel,
        create_memory_entry,
    };
    use local_rag_test_support::TempHome;

    use super::*;

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
            uuidv7_from(1000 + n, [0xCC; 10])
        }
    }

    fn uuid(seed: u8) -> String {
        let mut rand = [0u8; 10];
        rand[9] = seed;
        uuidv7_from(1000, rand).to_string()
    }

    fn open_state() -> (TempHome, StateDb) {
        let home = TempHome::new().expect("temp home");
        let layout = StoreLayout::new(home.join("local-rag"));
        layout.ensure().expect("ensure store tree");
        let db = StateDb::open(layout.state_db()).expect("open state.sqlite");
        (home, db)
    }

    /// Seed a standalone `observation_envelope` row so evidence citations
    /// resolve to something real (mirrors `crates/store/tests/memory.rs`'s
    /// own `seed_observation` helper).
    async fn seed_observation(db: &StateDb, observation_id: &str, evidence_kind: EvidenceKind) {
        let (oid, ek) = (
            observation_id.to_string(),
            evidence_kind.as_str().to_string(),
        );
        db.writer()
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO observation_envelope \
                       (observation_id, source_event_id, payload_hash, event_type, \
                        evidence_kind, trust, session_id) \
                     VALUES (?1, 'evt-1', 'deadbeef', 'Stop', ?2, 'normal', 'sess-1')",
                    params![oid, ek],
                )
            })
            .await
            .expect("seed observation envelope");
    }

    fn window_observation(
        observation_id: &str,
        evidence_kind: EvidenceKind,
        repo_id: Option<&str>,
        worktree_id: Option<&str>,
    ) -> WindowObservation {
        WindowObservation {
            observation_id: observation_id.to_string(),
            received_seq: 1,
            event_type: "Stop".to_string(),
            evidence_kind,
            trust: TrustLevel::Normal,
            session_id: "sess-1".to_string(),
            repo_id: repo_id.map(str::to_string),
            worktree_id: worktree_id.map(str::to_string),
            agent_id: None,
            commit_hash: None,
            short_evidence_excerpt: None,
            payload: None,
        }
    }

    fn create_raw(
        kind: &str,
        scope_kind: &str,
        canonical_key: Option<&str>,
        cites: Vec<String>,
    ) -> RawRouterOp {
        RawRouterOp::Create {
            kind: kind.to_string(),
            text: "we decided to use pnpm".to_string(),
            canonical_key: canonical_key.map(str::to_string),
            scope_kind: scope_kind.to_string(),
            confidence_signal: Some(Signal::High),
            importance_signal: Some(Signal::Medium),
            cites,
        }
    }

    async fn create_memory(
        db: &StateDb,
        memory_id: &str,
        kind: StoreMemoryKind,
        scope_kind: ScopeKind,
        scope_owner_id: &str,
        canonical_key: Option<&str>,
    ) {
        let (id, owner, key) = (
            memory_id.to_string(),
            scope_owner_id.to_string(),
            canonical_key.map(str::to_string),
        );
        db.writer()
            .transaction(move |tx| {
                create_memory_entry(
                    tx,
                    &NewMemoryEntry {
                        memory_id: &id,
                        kind,
                        text: "existing entry",
                        canonical_key: key.as_deref(),
                        scope_kind,
                        scope_owner_id: &owner,
                        confidence: 0.5,
                        importance: 0.5,
                        valid_from_tree: None,
                        last_verified_tree: None,
                        supersedes_id: None,
                    },
                    1000,
                )
            })
            .await
            .expect("create memory tx")
            .expect("create memory domain");
    }

    #[tokio::test]
    async fn create_fact_with_user_statement_evidence_materializes() {
        let (_home, db) = open_state();
        let obs_id = uuid(1);
        seed_observation(&db, &obs_id, EvidenceKind::UserStatement).await;
        let window = vec![window_observation(
            &obs_id,
            EvidenceKind::UserStatement,
            None,
            None,
        )];
        let by_id: HashMap<&str, &WindowObservation> = window
            .iter()
            .map(|o| (o.observation_id.as_str(), o))
            .collect();

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw("fact", "global", None, vec![obs_id.clone()]);
        let outcome = materialize(&read, &by_id, &window, &uuids, raw).expect("materialize");
        assert!(
            matches!(outcome, GeneratedOp::Materialize { .. }),
            "explicit user statement satisfies the durable gate, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn create_fact_backed_only_by_a_model_claim_is_downgraded() {
        let (_home, db) = open_state();
        let obs_id = uuid(2);
        seed_observation(&db, &obs_id, EvidenceKind::ModelClaim).await;
        let window = vec![window_observation(
            &obs_id,
            EvidenceKind::ModelClaim,
            None,
            None,
        )];
        let by_id: HashMap<&str, &WindowObservation> = window
            .iter()
            .map(|o| (o.observation_id.as_str(), o))
            .collect();

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw("fact", "global", None, vec![obs_id.clone()]);
        let outcome = materialize(&read, &by_id, &window, &uuids, raw).expect("materialize");
        match outcome {
            GeneratedOp::ProposeCandidate { conflicts, .. } => assert!(conflicts.is_empty()),
            other => panic!("expected ProposeCandidate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_fact_with_no_citations_is_downgraded() {
        let (_home, db) = open_state();
        let window: Vec<WindowObservation> = vec![];
        let by_id: HashMap<&str, &WindowObservation> = HashMap::new();

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw("fact", "global", None, vec![]);
        let outcome = materialize(&read, &by_id, &window, &uuids, raw).expect("materialize");
        assert!(matches!(outcome, GeneratedOp::ProposeCandidate { .. }));
    }

    #[tokio::test]
    async fn create_hypothesis_with_only_a_model_claim_still_materializes() {
        let (_home, db) = open_state();
        let obs_id = uuid(3);
        seed_observation(&db, &obs_id, EvidenceKind::ModelClaim).await;
        let window = vec![window_observation(
            &obs_id,
            EvidenceKind::ModelClaim,
            None,
            None,
        )];
        let by_id: HashMap<&str, &WindowObservation> = window
            .iter()
            .map(|o| (o.observation_id.as_str(), o))
            .collect();

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw("hypothesis", "global", None, vec![obs_id.clone()]);
        let outcome = materialize(&read, &by_id, &window, &uuids, raw).expect("materialize");
        assert!(
            matches!(outcome, GeneratedOp::Materialize { .. }),
            "neither gate applies to a hypothesis, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn create_with_a_canonical_key_conflict_is_downgraded_with_the_owner_named() {
        let (_home, db) = open_state();
        let existing_id = uuid(4);
        create_memory(
            &db,
            &existing_id,
            StoreMemoryKind::Fact,
            ScopeKind::Global,
            local_rag_store::GLOBAL_SCOPE_OWNER_ID,
            Some("storage-backend"),
        )
        .await;

        let obs_id = uuid(5);
        seed_observation(&db, &obs_id, EvidenceKind::UserStatement).await;
        let window = vec![window_observation(
            &obs_id,
            EvidenceKind::UserStatement,
            None,
            None,
        )];
        let by_id: HashMap<&str, &WindowObservation> = window
            .iter()
            .map(|o| (o.observation_id.as_str(), o))
            .collect();

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw(
            "fact",
            "global",
            Some("storage-backend"),
            vec![obs_id.clone()],
        );
        let outcome = materialize(&read, &by_id, &window, &uuids, raw).expect("materialize");
        match outcome {
            GeneratedOp::ProposeCandidate { conflicts, .. } => {
                assert_eq!(conflicts, vec![existing_id]);
            }
            other => panic!("expected ProposeCandidate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_with_an_invalid_kind_string_noops() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw("not-a-real-kind", "global", None, vec![]);
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(outcome, GeneratedOp::Noop);
    }

    /// D-048 regression: `scope_kind` now carries `#[serde(default)]`
    /// (`crate::schema`), so a `create` op missing it entirely deserializes
    /// with `scope_kind == ""` instead of failing the whole batch — this
    /// proves the end-to-end degrade path already handles that empty value
    /// exactly like any other out-of-domain string, same as the sibling test
    /// above for an invalid `kind`.
    #[tokio::test]
    async fn create_with_an_empty_scope_kind_noops() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw("fact", "", None, vec![]);
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(outcome, GeneratedOp::Noop);
    }

    /// D-051 regression: `confidence_signal`/`importance_signal` now carry
    /// `#[serde(default)]` as `Option<Signal>` (`crate::schema`), so a
    /// `create` op missing either one deserializes with `None` instead of
    /// failing the whole batch — this proves the end-to-end degrade path
    /// treats a missing signal exactly like an out-of-domain `scope_kind`
    /// above: `Noop` for this one op, never a fabricated value.
    #[tokio::test]
    async fn create_with_a_missing_confidence_signal_noops() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Create {
            kind: "fact".to_string(),
            text: "we decided to use pnpm".to_string(),
            canonical_key: None,
            scope_kind: "global".to_string(),
            confidence_signal: None,
            importance_signal: Some(Signal::Medium),
            cites: vec![],
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(outcome, GeneratedOp::Noop);
    }

    /// Same gap, `importance_signal` instead.
    #[tokio::test]
    async fn create_with_a_missing_importance_signal_noops() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Create {
            kind: "fact".to_string(),
            text: "we decided to use pnpm".to_string(),
            canonical_key: None,
            scope_kind: "global".to_string(),
            confidence_signal: Some(Signal::Medium),
            importance_signal: None,
            cites: vec![],
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(outcome, GeneratedOp::Noop);
    }

    /// Same gap, `supersede` variant — a different `handle_*` function with
    /// its own independent degrade check.
    #[tokio::test]
    async fn supersede_with_a_missing_confidence_signal_noops() {
        let (_home, db) = open_state();
        let id = uuid(15);
        create_memory(
            &db,
            &id,
            StoreMemoryKind::Hypothesis,
            ScopeKind::Global,
            local_rag_store::GLOBAL_SCOPE_OWNER_ID,
            None,
        )
        .await;

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Supersede {
            target_memory_id: id,
            new_kind: "fact".to_string(),
            new_text: "confirmed".to_string(),
            new_canonical_key: None,
            confidence_signal: None,
            importance_signal: Some(Signal::High),
            cites: vec![],
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(outcome, GeneratedOp::Noop);
    }

    #[tokio::test]
    async fn create_repository_scoped_with_no_repo_id_anywhere_noops() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw("fact", "repository", None, vec![]);
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(
            outcome,
            GeneratedOp::Noop,
            "cannot place a repository-scoped entry with no repo_id in the window"
        );
    }

    #[tokio::test]
    async fn a_hallucinated_citation_is_dropped_not_forwarded() {
        let (_home, db) = open_state();
        let window: Vec<WindowObservation> = vec![];
        let by_id: HashMap<&str, &WindowObservation> = HashMap::new();

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = create_raw("fact", "global", None, vec!["does-not-exist".to_string()]);
        let outcome = materialize(&read, &by_id, &window, &uuids, raw).expect("materialize");
        match outcome {
            GeneratedOp::ProposeCandidate {
                evidence_observation_ids,
                ..
            } => assert!(
                evidence_observation_ids.is_empty(),
                "a hallucinated id must never reach the op's evidence list"
            ),
            other => panic!("expected ProposeCandidate (no real evidence), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn propose_candidate_op_always_proposes_even_with_strong_evidence() {
        let (_home, db) = open_state();
        let obs_id = uuid(6);
        seed_observation(&db, &obs_id, EvidenceKind::UserStatement).await;
        let window = vec![window_observation(
            &obs_id,
            EvidenceKind::UserStatement,
            None,
            None,
        )];
        let by_id: HashMap<&str, &WindowObservation> = window
            .iter()
            .map(|o| (o.observation_id.as_str(), o))
            .collect();

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::ProposeCandidate {
            kind: "fact".to_string(),
            text: "maybe uses pnpm".to_string(),
            canonical_key: None,
            scope_kind: "global".to_string(),
            confidence_signal: Some(Signal::Low),
            importance_signal: Some(Signal::Low),
            cites: vec![obs_id],
        };
        let outcome = materialize(&read, &by_id, &window, &uuids, raw).expect("materialize");
        assert!(matches!(outcome, GeneratedOp::ProposeCandidate { .. }));
    }

    #[tokio::test]
    async fn noop_op_is_a_plain_noop() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Noop {
            reason: Some("just a question".to_string()),
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(outcome, GeneratedOp::Noop);
    }

    #[tokio::test]
    async fn reinforce_unknown_target_noops() {
        let (_home, db) = open_state();
        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Reinforce {
            target_memory_id: uuid(7),
            confidence_signal: None,
            cites: vec![],
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(outcome, GeneratedOp::Noop);
    }

    #[tokio::test]
    async fn reinforce_a_known_entry_uses_its_fresh_version_never_a_model_echoed_one() {
        let (_home, db) = open_state();
        let id = uuid(8);
        create_memory(
            &db,
            &id,
            StoreMemoryKind::Fact,
            ScopeKind::Global,
            local_rag_store::GLOBAL_SCOPE_OWNER_ID,
            None,
        )
        .await;
        // Bump the entry's own version behind the router's back, exactly
        // like a prior reinforce would.
        let bump_id = id.clone();
        db.writer()
            .transaction(move |tx| {
                tx.execute(
                    "UPDATE memory_entry SET entry_version = 7 WHERE memory_id = ?1",
                    params![bump_id],
                )
            })
            .await
            .expect("bump version");

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Reinforce {
            target_memory_id: id.clone(),
            confidence_signal: Some(Signal::High),
            cites: vec![],
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        match outcome {
            GeneratedOp::Materialize {
                operation:
                    ProposedOperation::Reinforce {
                        expected_version, ..
                    },
                ..
            } => assert_eq!(expected_version, 7),
            other => panic!("expected Materialize(Reinforce), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_on_a_fact_is_illegal_and_noops() {
        let (_home, db) = open_state();
        let id = uuid(9);
        create_memory(
            &db,
            &id,
            StoreMemoryKind::Fact,
            ScopeKind::Global,
            local_rag_store::GLOBAL_SCOPE_OWNER_ID,
            None,
        )
        .await;

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Resolve {
            target_memory_id: id,
            cites: vec![],
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(
            outcome,
            GeneratedOp::Noop,
            "fact/decision/convention/procedure never resolve, only task/question do"
        );
    }

    #[tokio::test]
    async fn resolve_on_a_task_materializes() {
        let (_home, db) = open_state();
        let id = uuid(10);
        create_memory(
            &db,
            &id,
            StoreMemoryKind::Task,
            ScopeKind::Global,
            local_rag_store::GLOBAL_SCOPE_OWNER_ID,
            None,
        )
        .await;

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Resolve {
            target_memory_id: id,
            cites: vec![],
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert!(matches!(outcome, GeneratedOp::Materialize { .. }));
    }

    #[tokio::test]
    async fn supersede_on_a_task_is_illegal_and_noops() {
        let (_home, db) = open_state();
        let id = uuid(11);
        create_memory(
            &db,
            &id,
            StoreMemoryKind::Task,
            ScopeKind::Global,
            local_rag_store::GLOBAL_SCOPE_OWNER_ID,
            None,
        )
        .await;

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Supersede {
            target_memory_id: id,
            new_kind: "fact".to_string(),
            new_text: "promoted".to_string(),
            new_canonical_key: None,
            confidence_signal: Some(Signal::High),
            importance_signal: Some(Signal::High),
            cites: vec![],
        };
        let outcome = materialize(&read, &HashMap::new(), &[], &uuids, raw).expect("materialize");
        assert_eq!(outcome, GeneratedOp::Noop);
    }

    #[tokio::test]
    async fn supersede_inherits_the_old_entrys_scope_and_drops_a_reused_canonical_key() {
        let (_home, db) = open_state();
        let repo_id = uuid(12);
        let old_id = uuid(13);
        create_memory(
            &db,
            &old_id,
            StoreMemoryKind::Hypothesis,
            ScopeKind::Repository,
            &repo_id,
            Some("storage-backend"),
        )
        .await;

        let obs_id = uuid(14);
        seed_observation(&db, &obs_id, EvidenceKind::UserStatement).await;
        let window = vec![window_observation(
            &obs_id,
            EvidenceKind::UserStatement,
            None,
            None,
        )];
        let by_id: HashMap<&str, &WindowObservation> = window
            .iter()
            .map(|o| (o.observation_id.as_str(), o))
            .collect();

        let read = db.open_read().expect("read conn");
        let uuids = SeqUuidV7::new();
        let raw = RawRouterOp::Supersede {
            target_memory_id: old_id.clone(),
            new_kind: "fact".to_string(),
            new_text: "confirmed: uses postgres".to_string(),
            new_canonical_key: Some("storage-backend".to_string()),
            confidence_signal: Some(Signal::High),
            importance_signal: Some(Signal::High),
            cites: vec![obs_id],
        };
        let outcome = materialize(&read, &by_id, &window, &uuids, raw).expect("materialize");
        match outcome {
            GeneratedOp::Materialize {
                operation:
                    ProposedOperation::Supersede {
                        old_memory_id,
                        new_scope_kind,
                        new_scope_owner_id,
                        new_canonical_key,
                        ..
                    },
                ..
            } => {
                assert_eq!(old_memory_id, old_id);
                assert_eq!(new_scope_kind, "repository");
                assert_eq!(new_scope_owner_id, repo_id);
                assert_eq!(
                    new_canonical_key, None,
                    "reusing the old entry's own key must be dropped, not forwarded"
                );
            }
            other => panic!("expected Materialize(Supersede), got {other:?}"),
        }
    }
}
