//! `local-rag doctor [--worktree <id>] [--json]` (spec 11 §6, T16-03) —
//! store-wide, read-only health report: lock, versions, permissions, cache
//! binding, orphan shard artifacts, and per-worktree FTS/dense head status.
//!
//! # `doctor` never mutates — the fixed call order this relies on
//!
//! Every normal constructor this crate has (`StateDb::open`, `CacheDb::open`,
//! `StoreLayout::ensure`/`local_rag::indexing::open_state`) either applies pending
//! migrations, rebuilds an incompatible cache, or **re-asserts** permissions
//! as a side effect of opening — exactly the machinery this command exists to
//! diagnose, not to run. `build_report` therefore follows one fixed order,
//! not incidental code layout:
//!
//! 1. **lock** — a pure file read (`daemon::read_store_lock_file`), no
//!    `flock` attempt.
//! 2. **permissions** — `stat`/`lstat` only (`StoreLayout::audit_permissions`),
//!    run before anything else so a later step's own `ensure_dir` re-assert
//!    (inside `StateDb`'s own open path, if this command ever took it) cannot
//!    silently erase the very fault this section exists to report.
//! 3. **versions** — a raw `SQLITE_OPEN_READ_ONLY` connection
//!    (`StateDb::diagnose_versions`), never `StateDb::open`/`migrate::run`.
//! 4. **cache binding** — likewise raw and read-only
//!    (`CacheDb::diagnose_binding`), never `CacheDb::open`.
//! 5. Only once versions confirms the store is compatible **and has nothing
//!    pending** does this command construct a real `StateDb::open` at all,
//!    for the **orphans** (three already-existing sweeps, always
//!    `dry_run: true`) and **heads** sections. Any other versions outcome
//!    (`NotInitialized`/`MissingBookkeeping`/`Fault`, or `Applied` with
//!    nonzero `pending`) reports both of those sections `Skipped` — opening
//!    `StateDb` in the `pending`-nonzero case specifically would silently
//!    apply the exact migrations this command exists to report as pending.
//!
//! `rebuild --fts`/`--dense` (T15-07, `cli::rebuild`) is untouched by this
//! task and is what an operator runs after reading a divergent head here —
//! `doctor` diagnoses, it does not repair (no `--fix` flag, deliberately: the
//! card and D-025 draw a hard line between the two).

use std::process::ExitCode;

use local_rag_core::identity::Uuid;
use local_rag_core::paths::{PathError, StoreLayout};
use local_rag_projection::{
    BruteForceProjectionStore, DenseCheckOutcome, ModelSwitchError, check_dense,
};
use local_rag_store::{
    CacheDb, CacheDiagnosis, DEFAULT_MODEL_SPACE_ID, FtsAvailability, FtsCheckOutcome,
    HousekeepingError, SHARD_DESTROY_GRACE_MS, STUCK_RUN_ATTEMPT_THRESHOLD, ShardSweepReport,
    StateDb, StuckRunRow, ValidationDepth, VersionDiagnosis, WorktreeIndexingStatus,
    all_worktree_ids, check_fts, current_worktree_path, generation_meta_for_worktree,
    indexing_status, managed_worktrees, requires_index_unavailable, run_expired_shard_sweep,
    run_orphan_shard_sweep, run_unreferenced_space_sweep, store_instance_uuid,
    stuck_consolidation_runs,
};

use local_rag::daemon::{StoreLockFileState, read_store_lock_file};

use super::freshness::{IndexFreshness, humanize_age};
use super::{fail, resolve_layout_and_config, system_now_ms};

const BIN: &str = "local-rag";

// ---------------------------------------------------------------------------
// Report shape
// ---------------------------------------------------------------------------

pub struct DoctorReport {
    pub lock: StoreLockFileState,
    pub permissions: Vec<PathError>,
    pub versions: Result<VersionDiagnosis, String>,
    pub cache_binding: Result<CacheDiagnosis, String>,
    pub orphans: OrphansFinding,
    pub heads: HeadsFinding,
    pub spool: SpoolFinding,
    /// X-008: is background indexing enrolled for each worktree, how old is
    /// what search serves, and is newer work built but stuck?
    pub indexing: IndexingFinding,
    /// D-071: consolidation runs that are being retried without converging,
    /// or that the retry machinery has given up on entirely.
    pub consolidation: ConsolidationFinding,
}

/// D-071: the consolidation section — `doctor` had none at all, which is why
/// a run on its 627th attempt could pin the GPU for eight hours while this
/// report still said `clean`. Non-empty means a human has to look: every row
/// is either not converging or permanently parked.
pub enum ConsolidationFinding {
    Skipped { reason: String },
    Checked(Vec<StuckRunRow>),
}

/// X-008: the background-indexing section — one entry per worktree.
///
/// Answers, per worktree, the three questions no command could answer before:
/// is it enrolled at all (`managed_worktree`, T20-01), how stale is the
/// generation search actually serves, and has a newer generation been built
/// without ever being switched on.
pub enum IndexingFinding {
    Skipped { reason: String },
    Checked(Vec<WorktreeIndexing>),
}

pub struct WorktreeIndexing {
    pub worktree_id: String,
    /// Current on-disk path, when the registry still has one.
    pub path: Option<String>,
    /// Enrolled in daemon-managed background indexing at all.
    pub managed: bool,
    /// `managed` **and** not paused — the supervisor's own run/skip axis.
    pub enabled: bool,
    pub freshness: IndexFreshness,
    /// The durable outcome of the last background cycle (X-006), if one ran.
    pub status: Option<WorktreeIndexingStatus>,
}

/// D-030: every known spool session's stalled-import diagnostic (spec 11 §4
/// `[FIXED concern]`: "a newer hook binary writing a newer format than the
/// running daemon supports is a reportable incompatibility, not silent
/// loss") — read-only, independent of the real import path
/// (`local_rag_store::diagnose_spool_tail`).
pub enum SpoolFinding {
    Skipped { reason: String },
    Checked(Vec<SpoolSessionFinding>),
}

pub struct SpoolSessionFinding {
    pub session_id: String,
    /// `Ok(None)` = healthy; `Ok(Some(reason))` = genuinely stalled on
    /// import (never silently retried); `Err(_)` = the diagnostic itself
    /// could not run.
    pub stalled_on: Result<Option<String>, String>,
}

pub enum OrphansFinding {
    Skipped {
        reason: String,
    },
    Checked {
        orphan_shard: ShardSweepReport,
        expired_shard: ShardSweepReport,
        unreferenced_space: ShardSweepReport,
    },
}

pub enum HeadsFinding {
    Skipped { reason: String },
    Checked(Vec<WorktreeHeads>),
}

pub struct WorktreeHeads {
    pub worktree_id: String,
    pub fts: Result<FtsCheckOutcome, String>,
    pub dense: Result<DenseCheckOutcome, String>,
    pub both_legs_unavailable: bool,
}

impl DoctorReport {
    /// Every section either found nothing to report or reports a benign
    /// bootstrap state (never-yet-initialized store/cache, a worktree never
    /// indexed on either leg) — the same "explicit, never silent" rule spec
    /// 02 §6 already gives degraded search responses, applied here to the
    /// aggregate report.
    fn is_clean(&self) -> bool {
        let lock_ok = matches!(
            self.lock,
            StoreLockFileState::Absent | StoreLockFileState::Parsed(_)
        );
        let permissions_ok = self.permissions.is_empty();

        // A store that has never been initialized (`local-rag init`/`index`
        // never run: no `state.sqlite` at all) is not a fault to report —
        // `cache_binding`/`orphans`/`heads` are all `Skipped`/`Err` for the
        // identical reason and add no further signal, so this is the one
        // case where lock/permissions alone decide cleanliness.
        if matches!(&self.versions, Ok(VersionDiagnosis::NotInitialized)) {
            return lock_ok && permissions_ok;
        }

        let versions_ok =
            matches!(&self.versions, Ok(VersionDiagnosis::Applied(r)) if r.pending.is_empty());
        let cache_ok = matches!(
            &self.cache_binding,
            Ok(CacheDiagnosis::Bound) | Ok(CacheDiagnosis::NotInitialized)
        );
        let orphans_ok = matches!(
            &self.orphans,
            OrphansFinding::Checked { orphan_shard, expired_shard, unreferenced_space }
                if orphan_shard.removed.is_empty()
                    && expired_shard.removed.is_empty()
                    && unreferenced_space.removed.is_empty()
        );
        // `both_legs_unavailable` (spec 02 §6's `requires_index_unavailable`)
        // is true even for a worktree that has simply never been indexed on
        // either leg yet — a legitimate bootstrap state, the per-worktree
        // analogue of `VersionDiagnosis::NotInitialized` above, not a fault.
        // It is not an independent gate here: a genuinely broken leg already
        // fails its own `matches!` arm below (neither `Divergent` nor `Err`
        // is `Valid`/`NoActive*`), so the two per-leg checks alone are
        // sufficient — `both_legs_unavailable` stays on `WorktreeHeads` only
        // as an informational signal for the human/JSON reader.
        let heads_ok = matches!(
            &self.heads,
            HeadsFinding::Checked(list) if list.iter().all(|w| {
                matches!(
                    w.fts,
                    Ok(FtsCheckOutcome::Valid) | Ok(FtsCheckOutcome::NoActiveGeneration)
                ) && matches!(
                    w.dense,
                    Ok(DenseCheckOutcome::Valid) | Ok(DenseCheckOutcome::NoActiveTuple)
                )
            })
        );
        let spool_ok = matches!(
            &self.spool,
            SpoolFinding::Checked(list) if list.iter().all(|s| matches!(s.stalled_on, Ok(None)))
        );
        // X-008, the owner's explicit decision: of everything the indexing
        // section reports, only a **stuck** generation counts as a fault — a
        // generation newer than the active one, built and then never switched
        // on, i.e. work the system performed and discarded. "Not enrolled" and
        // "never indexed" stay informational for exactly the reason the
        // `heads_ok` comment above already gives for its own equivalent case:
        // a bootstrap state is not a fault. `IndexFreshness::has_fault` is that
        // one rule, in one place.
        let indexing_ok = matches!(
            &self.indexing,
            IndexingFinding::Checked(list) if list.iter().all(|w| !w.freshness.has_fault())
        );
        // D-071: a stuck consolidation run is a fault for the same reason a
        // stuck generation is — work the system performed and could not
        // land. A dead-lettered run additionally blocks its whole session's
        // backlog until the binary is rebuilt.
        let consolidation_ok = matches!(
            &self.consolidation,
            ConsolidationFinding::Checked(list) if list.is_empty()
        );
        lock_ok
            && permissions_ok
            && versions_ok
            && cache_ok
            && orphans_ok
            && heads_ok
            && spool_ok
            && indexing_ok
            && consolidation_ok
    }
}

// ---------------------------------------------------------------------------
// CLI entry
// ---------------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub struct DoctorArgs {
    /// Report only this worktree's heads (all worktrees by default).
    #[arg(long)]
    worktree: Option<String>,
    /// Print the report as JSON instead of human-readable lines.
    #[arg(long)]
    json: bool,
}

pub fn run(args: DoctorArgs) -> ExitCode {
    let worktree_filter: Option<Uuid> = match args.worktree {
        Some(v) => match v.parse::<Uuid>() {
            Ok(id) => Some(id),
            Err(_) => return fail(BIN, &format!("{v:?} is not a valid worktree id")),
        },
        None => None,
    };
    let json = args.json;

    let (layout, _config) = match resolve_layout_and_config() {
        Ok(v) => v,
        Err(e) => return fail(BIN, &e),
    };

    let report = build_report(&layout, worktree_filter);
    let clean = report.is_clean();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_json(&report))
                .expect("doctor report always serializes")
        );
    } else {
        print_human(&report);
    }
    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// Composition — see the module doc for why this exact order is load-bearing
// ---------------------------------------------------------------------------

fn build_report(layout: &StoreLayout, worktree_filter: Option<Uuid>) -> DoctorReport {
    let lock = read_store_lock_file(layout);
    let permissions = layout.audit_permissions();
    let versions = StateDb::diagnose_versions(&layout.state_db(), local_rag_store::ALL)
        .map_err(|e| e.to_string());

    let store_ready = matches!(&versions, Ok(VersionDiagnosis::Applied(r)) if r.pending.is_empty());
    if !store_ready {
        let reason = describe_versions_blocker(&versions);
        return DoctorReport {
            lock,
            permissions,
            versions,
            cache_binding: Err(reason.clone()),
            orphans: OrphansFinding::Skipped {
                reason: reason.clone(),
            },
            heads: HeadsFinding::Skipped {
                reason: reason.clone(),
            },
            spool: SpoolFinding::Skipped {
                reason: reason.clone(),
            },
            indexing: IndexingFinding::Skipped {
                reason: reason.clone(),
            },
            consolidation: ConsolidationFinding::Skipped { reason },
        };
    }

    let state = match StateDb::open(layout.state_db()) {
        Ok(s) => s,
        Err(e) => {
            let reason = format!("could not open state.sqlite: {e}");
            return DoctorReport {
                lock,
                permissions,
                versions,
                cache_binding: Err(reason.clone()),
                orphans: OrphansFinding::Skipped {
                    reason: reason.clone(),
                },
                heads: HeadsFinding::Skipped {
                    reason: reason.clone(),
                },
                spool: SpoolFinding::Skipped {
                    reason: reason.clone(),
                },
                indexing: IndexingFinding::Skipped {
                    reason: reason.clone(),
                },
                consolidation: ConsolidationFinding::Skipped { reason },
            };
        }
    };
    let read = match state.open_read() {
        Ok(c) => c,
        Err(e) => {
            let reason = format!("could not open a read connection to state.sqlite: {e}");
            return DoctorReport {
                lock,
                permissions,
                versions,
                cache_binding: Err(reason.clone()),
                orphans: OrphansFinding::Skipped {
                    reason: reason.clone(),
                },
                heads: HeadsFinding::Skipped {
                    reason: reason.clone(),
                },
                spool: SpoolFinding::Skipped {
                    reason: reason.clone(),
                },
                indexing: IndexingFinding::Skipped {
                    reason: reason.clone(),
                },
                consolidation: ConsolidationFinding::Skipped { reason },
            };
        }
    };

    let cache_binding = match store_instance_uuid(&read) {
        Ok(Some(uuid)) => Ok(CacheDb::diagnose_binding(&layout.cache_db(), &uuid)),
        Ok(None) => Err("store_instance_uuid has never been recorded".to_string()),
        Err(e) => Err(format!("could not read store_instance_uuid: {e}")),
    };

    let orphans = build_orphans(&state, layout);
    let heads = build_heads(&read, layout, &cache_binding, worktree_filter);
    let spool = build_spool(&read, layout);
    let indexing = build_indexing(&read, worktree_filter);
    let consolidation = build_consolidation(&read);

    DoctorReport {
        lock,
        permissions,
        versions,
        cache_binding,
        orphans,
        heads,
        spool,
        indexing,
        consolidation,
    }
}

/// D-071: every consolidation run that needs a human, from the same read
/// connection every other section uses. One store call
/// (`stuck_consolidation_runs`), whose own doc carries the "stuck" rule — a
/// read that fails degrades this one section to `Skipped`, exactly as its
/// neighbours do, rather than blinding the whole report.
fn build_consolidation(read: &rusqlite::Connection) -> ConsolidationFinding {
    match stuck_consolidation_runs(read, local_rag_core::BUILD_ID, STUCK_RUN_ATTEMPT_THRESHOLD) {
        Ok(runs) => ConsolidationFinding::Checked(runs),
        Err(e) => ConsolidationFinding::Skipped {
            reason: format!("could not read consolidation runs: {e}"),
        },
    }
}

/// X-008: the background-indexing section, built alongside orphans/heads under
/// the same "only once the store is fully migrated" discipline.
///
/// Joins three already-existing reads per worktree — `managed_worktrees`
/// (T20-01's enrollment), `generation_meta_for_worktree` (the generation
/// history) and `indexing_status` (X-006's durable outcome) — so it adds no new
/// store query of its own. A per-worktree read that fails degrades that one
/// field to "unknown" rather than skipping the whole section: a corrupt row in
/// one worktree must not blind the report to the other worktrees.
fn build_indexing(read: &rusqlite::Connection, worktree_filter: Option<Uuid>) -> IndexingFinding {
    let worktree_ids: Vec<String> = match worktree_filter {
        Some(id) => vec![id.to_string()],
        None => match all_worktree_ids(read) {
            Ok(ids) => ids,
            Err(e) => {
                return IndexingFinding::Skipped {
                    reason: format!("could not list worktrees: {e}"),
                };
            }
        },
    };
    let managed = match managed_worktrees(read) {
        Ok(rows) => rows,
        Err(e) => {
            return IndexingFinding::Skipped {
                reason: format!("could not read the managed-project registry: {e}"),
            };
        }
    };

    let entries = worktree_ids
        .into_iter()
        .map(|worktree_id| {
            let enrollment = managed.iter().find(|m| m.worktree_id == worktree_id);
            let generations = generation_meta_for_worktree(read, &worktree_id).unwrap_or_default();
            WorktreeIndexing {
                path: current_worktree_path(read, &worktree_id).ok().flatten(),
                managed: enrollment.is_some(),
                enabled: enrollment.is_some_and(|m| m.enabled),
                freshness: IndexFreshness::from_generations(&generations),
                status: indexing_status(read, &worktree_id).ok().flatten(),
                worktree_id,
            }
        })
        .collect();
    IndexingFinding::Checked(entries)
}

/// Every known spool session's stalled-import diagnostic, read-only
/// (D-030). Built alongside orphans/heads, under the same "only once the
/// store is fully migrated" discipline this module's doc comment already
/// gives them — `spool_import_cursor` (T13-04, migration 7) is guaranteed
/// present by that point.
fn build_spool(read: &rusqlite::Connection, layout: &StoreLayout) -> SpoolFinding {
    let sessions = match local_rag_store::known_spool_sessions(layout) {
        Ok(sessions) => sessions,
        Err(e) => {
            return SpoolFinding::Skipped {
                reason: format!("could not list spool sessions: {e}"),
            };
        }
    };
    let findings = sessions
        .into_iter()
        .map(|session_id| {
            let stalled_on = local_rag_store::diagnose_spool_tail(read, layout, &session_id)
                .map_err(|e| e.to_string());
            SpoolSessionFinding {
                session_id,
                stalled_on,
            }
        })
        .collect();
    SpoolFinding::Checked(findings)
}

fn describe_versions_blocker(versions: &Result<VersionDiagnosis, String>) -> String {
    match versions {
        Ok(VersionDiagnosis::NotInitialized) => "store not yet initialized".to_string(),
        Ok(VersionDiagnosis::MissingBookkeeping) => {
            "state.sqlite exists but is not a recognized store".to_string()
        }
        Ok(VersionDiagnosis::Applied(r)) => {
            format!(
                "{} migration(s) pending; run `local-rag serve`/`index` first",
                r.pending.len()
            )
        }
        Ok(VersionDiagnosis::Fault(e)) => e.to_string(),
        Err(e) => e.clone(),
        Ok(_) => "unknown version diagnosis".to_string(),
    }
}

/// Always three dry-run sweeps (spec 05 §8, D-007, D-011) — the file-system
/// "orphan artifacts" spec 11 §6 names, as opposed to `gc`'s three additional
/// DB-row sweeps (spool sessions/payload TTL/candidate expiry), which are not
/// artifacts in that sense and remain `gc`'s alone.
fn build_orphans(state: &StateDb, layout: &StoreLayout) -> OrphansFinding {
    let orphan_shard = run_orphan_shard_sweep(state, layout, true);
    let expired_shard =
        run_expired_shard_sweep(state, layout, system_now_ms(), SHARD_DESTROY_GRACE_MS, true);
    let unreferenced_space = run_unreferenced_space_sweep(state, layout, true);
    match (orphan_shard, expired_shard, unreferenced_space) {
        (Ok(orphan_shard), Ok(expired_shard), Ok(unreferenced_space)) => OrphansFinding::Checked {
            orphan_shard,
            expired_shard,
            unreferenced_space,
        },
        (orphan_shard, expired_shard, unreferenced_space) => {
            let reason: Vec<String> = [
                orphan_shard.err(),
                expired_shard.err(),
                unreferenced_space.err(),
            ]
            .into_iter()
            .flatten()
            .map(|e: HousekeepingError| e.to_string())
            .collect();
            OrphansFinding::Skipped {
                reason: reason.join("; "),
            }
        }
    }
}

/// Per-worktree FTS/dense head status (spec 06 §4, 05 §6) — every worktree
/// registered in the store, or just `worktree_filter` if given.
fn build_heads(
    read: &rusqlite::Connection,
    layout: &StoreLayout,
    cache_binding: &Result<CacheDiagnosis, String>,
    worktree_filter: Option<Uuid>,
) -> HeadsFinding {
    let worktree_ids: Vec<String> = match worktree_filter {
        Some(id) => vec![id.to_string()],
        None => match all_worktree_ids(read) {
            Ok(ids) => ids,
            Err(e) => {
                return HeadsFinding::Skipped {
                    reason: format!("could not list worktrees: {e}"),
                };
            }
        },
    };

    let model_space_id: Uuid = DEFAULT_MODEL_SPACE_ID
        .parse()
        .expect("DEFAULT_MODEL_SPACE_ID is a valid UUID");
    let shard_params =
        local_rag_projection::params_for_model_space(read, &model_space_id).map_err(|e| match e {
            ModelSwitchError::NoShardParams { model_space_id } => format!(
                "model space {model_space_id} has no code_raw representation registered yet"
            ),
            other => other.to_string(),
        });

    let cache_read = match cache_binding {
        Ok(CacheDiagnosis::Bound) => match CacheDb::open_read_only(&layout.cache_db()) {
            Ok(conn) => Ok(conn),
            Err(e) => Err(format!("could not open cache.sqlite: {e}")),
        },
        Ok(other) => Err(format!("cache.sqlite is not bound and healthy: {other:?}")),
        Err(e) => Err(e.clone()),
    };

    let store = BruteForceProjectionStore::new();
    let mut worktree_heads = Vec::with_capacity(worktree_ids.len());
    for worktree_id_str in worktree_ids {
        let Ok(worktree_id) = worktree_id_str.parse::<Uuid>() else {
            // Registry-minted ids are always valid UUIDs by construction
            // (spec 01 §5); skip defensively rather than panic on a store this
            // command itself never wrote.
            continue;
        };

        let dense = match shard_params {
            Ok(params) => {
                let shard_dir =
                    local_rag_projection::shard_dir(layout, &worktree_id, &model_space_id);
                check_dense(read, &store, &shard_dir, params, worktree_id)
                    .map_err(|e| e.to_string())
            }
            Err(ref e) => Err(e.clone()),
        };
        let fts = match &cache_read {
            Ok(cache_conn) => check_fts(read, cache_conn, &worktree_id_str, ValidationDepth::Cheap)
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.clone()),
        };

        let fts_availability = match &fts {
            Ok(FtsCheckOutcome::NoActiveGeneration) => FtsAvailability::Unavailable(None),
            Ok(FtsCheckOutcome::Valid) => FtsAvailability::Valid,
            Ok(FtsCheckOutcome::Divergent { divergence, .. }) => {
                FtsAvailability::Unavailable(Some(divergence.clone()))
            }
            Err(_) => FtsAvailability::Unavailable(None),
        };
        let dense_available = matches!(dense, Ok(DenseCheckOutcome::Valid));
        let both_legs_unavailable = requires_index_unavailable(&fts_availability, dense_available);

        worktree_heads.push(WorktreeHeads {
            worktree_id: worktree_id_str,
            fts,
            dense,
            both_legs_unavailable,
        });
    }

    HeadsFinding::Checked(worktree_heads)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn permission_finding_json(e: &PathError) -> serde_json::Value {
    serde_json::json!({"detail": e.to_string()})
}

fn shard_sweep_json(r: &ShardSweepReport) -> serde_json::Value {
    serde_json::json!({
        "removed": r.removed,
        "retained": r.retained,
    })
}

fn report_json(report: &DoctorReport) -> serde_json::Value {
    let lock = match &report.lock {
        StoreLockFileState::Absent => serde_json::json!({"state": "absent"}),
        StoreLockFileState::Corrupt => serde_json::json!({"state": "corrupt"}),
        StoreLockFileState::Parsed(info) => serde_json::json!({
            "state": "parsed",
            "pid": info.pid,
            "pid_alive": local_rag_core::process::pid_exists(info.pid),
            "instance_uuid": info.instance_uuid,
            "ready": info.ready,
        }),
    };

    let versions = match &report.versions {
        Ok(VersionDiagnosis::NotInitialized) => serde_json::json!({"state": "not_initialized"}),
        Ok(VersionDiagnosis::MissingBookkeeping) => {
            serde_json::json!({"state": "missing_bookkeeping"})
        }
        Ok(VersionDiagnosis::Applied(r)) => serde_json::json!({
            "state": "applied",
            "store_version": r.store_version,
            "binary_max_version": r.binary_max_version,
            "pending": r.pending,
        }),
        Ok(VersionDiagnosis::Fault(e)) => {
            serde_json::json!({"state": "fault", "detail": e.to_string()})
        }
        Err(e) => serde_json::json!({"state": "error", "detail": e}),
        Ok(_) => serde_json::json!({"state": "unknown"}),
    };

    let cache_binding = match &report.cache_binding {
        Ok(CacheDiagnosis::NotInitialized) => serde_json::json!({"state": "not_initialized"}),
        Ok(CacheDiagnosis::Unreadable) => serde_json::json!({"state": "unreadable"}),
        Ok(CacheDiagnosis::WrongBinding { found }) => {
            serde_json::json!({"state": "wrong_binding", "found": found})
        }
        Ok(CacheDiagnosis::IncompatibleSchema { found, binary }) => serde_json::json!({
            "state": "incompatible_schema", "found": found, "binary": binary,
        }),
        Ok(CacheDiagnosis::Bound) => serde_json::json!({"state": "bound"}),
        Err(e) => serde_json::json!({"state": "error", "detail": e}),
        Ok(_) => serde_json::json!({"state": "unknown"}),
    };

    let orphans = match &report.orphans {
        OrphansFinding::Skipped { reason } => serde_json::json!({"skipped": reason}),
        OrphansFinding::Checked {
            orphan_shard,
            expired_shard,
            unreferenced_space,
        } => serde_json::json!({
            "orphan_shard": shard_sweep_json(orphan_shard),
            "expired_shard": shard_sweep_json(expired_shard),
            "unreferenced_space": shard_sweep_json(unreferenced_space),
        }),
    };

    let heads = match &report.heads {
        HeadsFinding::Skipped { reason } => serde_json::json!({"skipped": reason}),
        HeadsFinding::Checked(list) => serde_json::Value::Array(
            list.iter()
                .map(|w| {
                    serde_json::json!({
                        "worktree_id": w.worktree_id,
                        "fts": match &w.fts {
                            Ok(outcome) => serde_json::json!({"ok": format!("{outcome:?}")}),
                            Err(e) => serde_json::json!({"error": e}),
                        },
                        "dense": match &w.dense {
                            Ok(outcome) => serde_json::json!({"ok": format!("{outcome:?}")}),
                            Err(e) => serde_json::json!({"error": e}),
                        },
                        "both_legs_unavailable": w.both_legs_unavailable,
                    })
                })
                .collect(),
        ),
    };

    let spool = match &report.spool {
        SpoolFinding::Skipped { reason } => serde_json::json!({"skipped": reason}),
        SpoolFinding::Checked(list) => serde_json::Value::Array(
            list.iter()
                .map(|s| {
                    serde_json::json!({
                        "session_id": s.session_id,
                        "stalled_on": match &s.stalled_on {
                            Ok(None) => serde_json::Value::Null,
                            Ok(Some(reason)) => serde_json::Value::String(reason.clone()),
                            Err(e) => serde_json::json!({"error": e}),
                        },
                    })
                })
                .collect(),
        ),
    };

    let consolidation = match &report.consolidation {
        ConsolidationFinding::Skipped { reason } => serde_json::json!({"skipped": reason}),
        ConsolidationFinding::Checked(list) => serde_json::json!({
            "stuck_runs": list
                .iter()
                .map(|r| serde_json::json!({
                    "run_id": r.run_id,
                    "session_id": r.session_id,
                    "attempt_count": r.attempt_count,
                    "dead_lettered": r.dead_lettered,
                    "last_failure_kind": r.last_failure_kind,
                    "last_failure_reason": r.last_failure_reason,
                    "from_received_seq": r.from_received_seq,
                    "to_received_seq": r.to_received_seq,
                }))
                .collect::<Vec<_>>(),
        }),
    };

    let indexing = match &report.indexing {
        IndexingFinding::Skipped { reason } => serde_json::json!({"skipped": reason}),
        IndexingFinding::Checked(list) => serde_json::Value::Array(
            list.iter()
                .map(|w| {
                    serde_json::json!({
                        "worktree_id": w.worktree_id,
                        "path": w.path,
                        "managed": w.managed,
                        "enabled": w.enabled,
                        "active_generation_number": w.freshness.active.as_ref().map(|(_, n, _)| *n),
                        "active_generation_created_at":
                            w.freshness.active.as_ref().and_then(|(_, _, ms)| *ms),
                        "generations_total": w.freshness.total,
                        "stuck_generations": w
                            .freshness
                            .stuck_newer
                            .iter()
                            .map(|s| serde_json::json!({
                                "generation_id": s.generation_id,
                                "generation_number": s.generation_number,
                                "state": s.state.as_str(),
                            }))
                            .collect::<Vec<_>>(),
                        "last_success_at": w.status.as_ref().and_then(|s| s.last_success_at),
                        "last_attempt_at": w.status.as_ref().and_then(|s| s.last_attempt_at),
                        "consecutive_failures": w.status.as_ref().map(|s| s.consecutive_failures),
                        "last_error": w.status.as_ref().and_then(|s| s.last_error.clone()),
                    })
                })
                .collect(),
        ),
    };

    serde_json::json!({
        "clean": report.is_clean(),
        "lock": lock,
        "permissions": report.permissions.iter().map(permission_finding_json).collect::<Vec<_>>(),
        "versions": versions,
        "cache_binding": cache_binding,
        "orphans": orphans,
        "heads": heads,
        "spool": spool,
        "indexing": indexing,
        "consolidation": consolidation,
    })
}

fn print_human(report: &DoctorReport) {
    println!(
        "{BIN} doctor: {}",
        if report.is_clean() {
            "clean"
        } else {
            "issues found"
        }
    );

    match &report.lock {
        StoreLockFileState::Absent => println!("lock: no daemon has ever run against this store"),
        StoreLockFileState::Corrupt => println!("lock: store.lock exists but could not be parsed"),
        StoreLockFileState::Parsed(info) => println!(
            "lock: held by pid {} (instance {}, ready={}, alive={})",
            info.pid,
            info.instance_uuid,
            info.ready,
            local_rag_core::process::pid_exists(info.pid)
        ),
    }

    if report.permissions.is_empty() {
        println!("permissions: ok");
    } else {
        for finding in &report.permissions {
            println!("permissions: {finding}");
        }
    }

    match &report.versions {
        Ok(VersionDiagnosis::NotInitialized) => println!("versions: store not yet initialized"),
        Ok(VersionDiagnosis::MissingBookkeeping) => {
            println!("versions: state.sqlite is not a recognized store")
        }
        Ok(VersionDiagnosis::Applied(r)) if r.pending.is_empty() => {
            println!("versions: up to date (v{})", r.store_version)
        }
        Ok(VersionDiagnosis::Applied(r)) => println!(
            "versions: {} pending ({:?}), store at v{}",
            r.pending.len(),
            r.pending,
            r.store_version
        ),
        Ok(VersionDiagnosis::Fault(e)) => println!("versions: {e}"),
        Err(e) => println!("versions: {e}"),
        Ok(_) => println!("versions: unknown diagnosis"),
    }

    match &report.cache_binding {
        Ok(CacheDiagnosis::NotInitialized) => println!("cache: not yet initialized"),
        Ok(CacheDiagnosis::Unreadable) => println!("cache: unreadable"),
        Ok(CacheDiagnosis::WrongBinding { found }) => {
            println!("cache: bound to a different store ({found})")
        }
        Ok(CacheDiagnosis::IncompatibleSchema { found, binary }) => {
            println!("cache: schema v{found}, this binary needs v{binary}")
        }
        Ok(CacheDiagnosis::Bound) => println!("cache: bound"),
        Err(e) => println!("cache: {e}"),
        Ok(_) => println!("cache: unknown diagnosis"),
    }

    match &report.orphans {
        OrphansFinding::Skipped { reason } => println!("orphans: skipped ({reason})"),
        OrphansFinding::Checked {
            orphan_shard,
            expired_shard,
            unreferenced_space,
        } => {
            println!(
                "orphans: {} orphan shard dir(s), {} expired, {} unreferenced model-space dir(s)",
                orphan_shard.removed.len(),
                expired_shard.removed.len(),
                unreferenced_space.removed.len(),
            );
        }
    }

    match &report.heads {
        HeadsFinding::Skipped { reason } => println!("heads: skipped ({reason})"),
        HeadsFinding::Checked(list) if list.is_empty() => {
            println!("heads: no worktrees registered")
        }
        HeadsFinding::Checked(list) => {
            for w in list {
                let fts = match &w.fts {
                    Ok(outcome) => format!("{outcome:?}"),
                    Err(e) => format!("error: {e}"),
                };
                let dense = match &w.dense {
                    Ok(outcome) => format!("{outcome:?}"),
                    Err(e) => format!("error: {e}"),
                };
                println!(
                    "heads: {} fts={fts} dense={dense}{}",
                    w.worktree_id,
                    if w.both_legs_unavailable {
                        " [BOTH LEGS UNAVAILABLE]"
                    } else {
                        ""
                    }
                );
            }
        }
    }

    match &report.spool {
        SpoolFinding::Skipped { reason } => println!("spool: skipped ({reason})"),
        SpoolFinding::Checked(list) if list.is_empty() => {
            println!("spool: no sessions")
        }
        SpoolFinding::Checked(list) => {
            for s in list {
                match &s.stalled_on {
                    Ok(None) => println!("spool: {} ok", s.session_id),
                    Ok(Some(reason)) => {
                        println!("spool: {} STALLED: {reason}", s.session_id)
                    }
                    Err(e) => println!("spool: {} error: {e}", s.session_id),
                }
            }
        }
    }

    print_indexing(&report.indexing);
    print_consolidation(&report.consolidation);
}

/// D-071's `consolidation:` section — the memory pillar's own "is anything
/// wedged?" line. Silent-but-present when healthy (one `ok` line, like
/// `permissions:`), loud and per-run when not: during the D-069 incident this
/// report had nothing to say at all while one run was on its 627th attempt.
fn print_consolidation(finding: &ConsolidationFinding) {
    match finding {
        ConsolidationFinding::Skipped { reason } => {
            println!("consolidation: skipped — {reason}")
        }
        ConsolidationFinding::Checked(list) if list.is_empty() => {
            println!("consolidation: ok — no run is stuck or dead-lettered")
        }
        ConsolidationFinding::Checked(list) => {
            for r in list {
                let verdict = if r.dead_lettered {
                    "DEAD-LETTERED on this build — nothing retries it until the binary changes"
                } else {
                    "STUCK: retried without converging"
                };
                println!(
                    "consolidation: run {} session {} received_seq {}..={} — {} attempt(s), {}",
                    r.run_id,
                    r.session_id,
                    r.from_received_seq,
                    r.to_received_seq,
                    r.attempt_count,
                    verdict,
                );
                if let Some(reason) = &r.last_failure_reason {
                    let kind = r.last_failure_kind.as_deref().unwrap_or("unclassified");
                    println!("  last failure ({kind}): {reason}");
                }
            }
        }
    }
}

/// X-008's `indexing:` section — for each worktree: enrolled or not, how old the
/// served generation is, what the last background cycle did, and (loudly) any
/// generation built but never switched on.
fn print_indexing(finding: &IndexingFinding) {
    let now_ms = system_now_ms();
    match finding {
        IndexingFinding::Skipped { reason } => println!("indexing: skipped — {reason}"),
        IndexingFinding::Checked(list) if list.is_empty() => {
            println!("indexing: no worktrees registered")
        }
        IndexingFinding::Checked(list) => {
            for w in list {
                let path = w.path.as_deref().unwrap_or("(no current path)");
                // "not enrolled" is the single most common reason a user thinks
                // indexing is broken, so it leads the line and carries the fix.
                let enrollment = match (w.managed, w.enabled) {
                    (false, _) => "NOT ENROLLED (run `local-rag project add <path>`)".to_string(),
                    (true, false) => "enrolled but PAUSED (`local-rag project enable`)".to_string(),
                    (true, true) => "enrolled".to_string(),
                };
                println!("indexing: {} {path} — {enrollment}", w.worktree_id);

                match &w.freshness.active {
                    Some((_, number, created_ms)) => {
                        let age = created_ms
                            .map(|ms| humanize_age(now_ms, ms))
                            .unwrap_or_else(|| "age unknown".to_string());
                        println!("  serving generation #{number}, built {age}");
                    }
                    None if w.freshness.total == 0 => {
                        println!("  never indexed — nothing is being served")
                    }
                    None => println!(
                        "  no active generation, though {} exist(s) on record",
                        w.freshness.total
                    ),
                }

                if let Some(status) = &w.status {
                    if let Some(ms) = status.last_success_at {
                        println!("  last successful cycle {}", humanize_age(now_ms, ms));
                    }
                    if status.consecutive_failures > 0 {
                        println!(
                            "  {} consecutive failure(s); last error: {}",
                            status.consecutive_failures,
                            status.last_error.as_deref().unwrap_or("(none recorded)"),
                        );
                    }
                }

                for s in &w.freshness.stuck_newer {
                    println!(
                        "  STUCK: generation #{} is {} but never became active — \
                         built work that is not being served",
                        s.generation_number,
                        s.state.as_str(),
                    );
                }
            }
        }
    }
}
