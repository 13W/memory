//! Retention/GC of the canonical source (spec 06 §5 `[FIXED]`), group 06.
//!
//! Retention is mark-and-sweep, and both halves live here:
//!
//! - the **mark phase** (T06-01) — the pure [`mark_pins`] plus its DB readers
//!   ([`generation_meta_for_worktree`], [`pinned_generation_roots`]) — computes the
//!   set of **pinned** generations a sweep must not delete;
//! - the **sweep phase** (T06-02) — [`run_sweep`] / [`plan_sweep`] — deletes the
//!   unpinned generations and the content graph they orphan, in batches of
//!   [`SWEEP_BATCH_ROWS`] rows/tx (spec 03 §3), walking the
//!   `generation → file_revision` reachability closure so a content-shared revision
//!   survives until its last referencing generation is swept.
//!
//! The mark phase writes nothing; the sweep phase mutates only through the single
//! bounded writer ([`StateWriter::transaction`](crate::StateWriter)), one committed
//! transaction per batch.
//!
//! # Pin roots (spec 06 §5)
//!
//! - `active` + `building`/`projection_ready` (the projection target) generations
//!   are pinned **unconditionally**;
//! - among `retiring` generations, the retention window keeps the **last `K`** by
//!   `generation_number` **or** those created within the window **`T`** — the two
//!   are a **union** (`OR`), the most protective reading for the spec's
//!   "rollback/debug" intent;
//! - generations referenced by memory evidence / audit / export are pinned;
//! - active (non-expired) rebuild/embedding job leases pin their generation.
//!
//! # Design decisions (`[SPEC]`, closing gaps the normative text leaves open)
//!
//! - **`failed` generations are not pinned by retention.** The spec's pin list
//!   names only "retired" generations (state `retiring`) and 04 §1 marks both
//!   `retiring` and `failed` as GC targets, not pin roots. A failed build's
//!   *shared* content survives via the `active` generation's references anyway
//!   (structural sharing, 06 §2); only its genuinely orphaned rows are swept.
//!   `failed` can still be pinned transitively by an external reference or a lease.
//! - **The window `T` is measured against `created_at`.** The `generation` table
//!   has no `retired_at` column (spec 03 §2.1) and adding one is a numbered
//!   migration — out of scope for a pure mark phase. `created_at` (birth time) is
//!   the only per-generation anchor today; a precise `retired_at` is a possible
//!   future migration, not needed now. `K` (last-K by number) needs no timestamp
//!   and is the primary mechanism.
//! - **`K` and `T` stay `[OPEN]` (O6).** They are read from
//!   [`StorageConfig`](local_rag_core::config::StorageConfig) via
//!   [`RetentionParams::from_storage_config`]; the current defaults (`K = 2`,
//!   `T = 168 h`) are provisional, not normative.
//!
//! # Seams for not-yet-built subsystems
//!
//! Memory evidence, audit, and export (groups 14/16) and the reconcile/embedding
//! job-lease table do not exist yet. Their contributions enter through
//! [`ExternalPins`], which defaults to empty — so today the mark reduces to the
//! generation-state and `K`/`T` roots, and later groups feed real references
//! without changing this algorithm.
//!
//! # Purity & determinism
//!
//! [`mark_pins`] is a pure function of an explicit `now_ms: i64` with no I/O and no
//! clock read — the codebase idiom (the reconcile `Debouncer`,
//! [`check_transition`](crate::registry::GenerationState::check_transition),
//! `build_generation`). It returns [`BTreeSet`]s, so the output is sorted and
//! independent of input order. The thin DB readers
//! ([`generation_meta_for_worktree`], [`pinned_generation_roots`]) load a
//! worktree's rows and delegate to the pure core, mirroring the "guarded read →
//! pure compute" split of
//! [`transition_generation`](crate::registry::transition_generation).

use std::collections::BTreeSet;

use rusqlite::types::Type;
use rusqlite::{Connection, Error, params};

use local_rag_core::config::StorageConfig;

use crate::registry::GenerationState;
use crate::state::{StateDb, WriteError};

/// Milliseconds per hour, for the `[storage].retired_generations_ttl_h` → window
/// conversion.
const MS_PER_HOUR: i64 = 3_600_000;

/// A snapshot of the `generation`-row fields the mark phase needs (spec 03 §2.1).
///
/// This is exactly the projection the pure [`mark_pins`] consumes, decoupled from
/// the database so the retention policy is table-testable without a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationMeta {
    /// The generation's stable id (UUIDv7).
    pub generation_id: String,
    /// Per-worktree monotone number; "last `K`" is the `K` largest of these among
    /// `retiring` generations.
    pub generation_number: i64,
    /// Lifecycle state (spec 04 §1) — decides whether the generation is an
    /// unconditional root, a retention candidate, or ignored.
    pub state: GenerationState,
    /// Birth time, Unix milliseconds; the window `T` is measured against this
    /// (`[SPEC]`, see module docs — there is no `retired_at`).
    pub created_at: i64,
}

/// The tuning parameters for the retention window (the spec's `K` and `T`).
///
/// Kept separate from [`StorageConfig`](local_rag_core::config::StorageConfig) so
/// the pure policy can be exercised with raw boundary values; build it from config
/// with [`RetentionParams::from_storage_config`]. Both values are `[OPEN]` (O6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionParams {
    /// `K`: keep the last `keep_last_k` `retiring` generations by number.
    pub keep_last_k: u32,
    /// `T`: keep `retiring` generations created within this many milliseconds of
    /// `now_ms`.
    pub window_ms: i64,
}

impl RetentionParams {
    /// Read `K`/`T` from the `[storage]` config (spec 02 §3.1): `K` verbatim, `T`
    /// converted hours → milliseconds (saturating, so an absurd config can never
    /// overflow). The config defaults are provisional (`[OPEN]` O6), not normative.
    pub fn from_storage_config(cfg: &StorageConfig) -> Self {
        let hours = i64::try_from(cfg.retired_generations_ttl_h).unwrap_or(i64::MAX);
        RetentionParams {
            keep_last_k: cfg.retired_generations_keep,
            window_ms: hours.saturating_mul(MS_PER_HOUR),
        }
    }
}

/// An active job lease that temporarily pins its generation (spec 06 §5: "active
/// rebuild/embedding job leases (temporary pins)").
///
/// A lease pins its `generation_id` only while it has not expired
/// (`lease_until_ms > now_ms`); an expired lease pins nothing. The backing table
/// is a later group — today leases are supplied through [`ExternalPins`] by tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLease {
    /// The generation the lease holds open.
    pub generation_id: String,
    /// Lease expiry, Unix milliseconds; strictly-greater-than `now_ms` still pins.
    pub lease_until_ms: i64,
}

/// Pin contributions from subsystems that do not exist yet (memory evidence /
/// audit / export — groups 14/16) plus active job leases.
///
/// Defaults to empty: today the mark phase has no external references, so
/// [`mark_pins`] reduces to the generation-state and `K`/`T` roots. Later groups
/// populate these fields without changing the mark algorithm.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalPins {
    /// Generations referenced by memory evidence / audit / export.
    pub referenced_generations: BTreeSet<String>,
    /// `file_revision`s referenced directly (not via a generation). Passed through
    /// to [`PinRoots`] unchanged; the sweep unions them with the generation-reachable
    /// revisions in T06-02.
    pub referenced_file_revisions: BTreeSet<String>,
    /// Active job leases (see [`JobLease`]); only non-expired ones pin.
    pub leases: Vec<JobLease>,
}

/// The result of the mark phase: the pinned generation roots plus the directly
/// referenced `file_revision`s carried through from [`ExternalPins`].
///
/// The `generation → file_revision` reachability closure (which revisions survive
/// because a pinned generation's `generation_file` rows reference them) is **not**
/// computed here — that is the sweep's job in T06-02. This type carries only the
/// *roots*.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinRoots {
    /// Pinned generation ids (sorted, deduplicated).
    pub generations: BTreeSet<String>,
    /// Directly referenced `file_revision` ids, passed through from
    /// [`ExternalPins::referenced_file_revisions`].
    pub referenced_file_revisions: BTreeSet<String>,
}

/// Compute the pinned generation roots for one worktree's generations (spec 06 §5).
///
/// Pure and deterministic: no I/O, no clock — time enters only as `now_ms`. The
/// caller must pass the generations of a **single** worktree (the "last `K`"
/// counts within a worktree); [`pinned_generation_roots`] enforces this by loading
/// per `worktree_id`.
///
/// See the module docs for the pin-root rules and the `[SPEC]` decisions
/// (`failed` not pinned; window `T` against `created_at`; `K`/`T` are the union).
pub fn mark_pins(
    gens: &[GenerationMeta],
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
) -> PinRoots {
    let mut generations = BTreeSet::new();

    // (a) Unconditional state roots: active + building/projection-target.
    for g in gens {
        match g.state {
            GenerationState::Active
            | GenerationState::Building
            | GenerationState::ProjectionReady => {
                generations.insert(g.generation_id.clone());
            }
            // `retiring` is handled by the retention window below; `failed` is not a
            // retention candidate ([SPEC], see module docs).
            GenerationState::Retiring | GenerationState::Failed => {}
        }
    }

    // (b) Retention window over `retiring` generations: union of "last K by number"
    // and "created within T of now". `window_floor` is the earliest `created_at`
    // still inside the window; `saturating_sub` keeps a huge `T` from underflowing
    // (it then pins every candidate).
    let window_floor = now_ms.saturating_sub(params.window_ms);
    let mut retiring: Vec<&GenerationMeta> = gens
        .iter()
        .filter(|g| g.state == GenerationState::Retiring)
        .collect();
    // Deterministic ordering: number desc, id as a stable tie-break (numbers are
    // unique per worktree, so the tie-break only matters for defensive determinism).
    retiring.sort_by(|a, b| {
        b.generation_number
            .cmp(&a.generation_number)
            .then_with(|| a.generation_id.cmp(&b.generation_id))
    });
    let keep_last_k = params.keep_last_k as usize;
    for (rank, g) in retiring.iter().enumerate() {
        let within_last_k = rank < keep_last_k;
        let within_window = g.created_at >= window_floor;
        if within_last_k || within_window {
            generations.insert(g.generation_id.clone());
        }
    }

    // (c) External references (memory evidence / audit / export).
    for id in &external.referenced_generations {
        generations.insert(id.clone());
    }

    // (d) Non-expired job leases pin their generation.
    for lease in &external.leases {
        if lease.lease_until_ms > now_ms {
            generations.insert(lease.generation_id.clone());
        }
    }

    PinRoots {
        generations,
        referenced_file_revisions: external.referenced_file_revisions.clone(),
    }
}

/// Load the [`GenerationMeta`] rows for one worktree, ascending by number (spec 03
/// §2.1). Worktree isolation is the `WHERE worktree_id = ?1` clause — the codebase
/// idiom ([`active_generations`](crate::registry::active_generations)).
///
/// A stored `state` outside the CHECK domain (corruption) surfaces as
/// [`rusqlite::Error::FromSqlConversionFailure`], never a silent default — the same
/// idiom as [`generation_state`](crate::registry::generation_state).
pub fn generation_meta_for_worktree(
    conn: &Connection,
    worktree_id: &str,
) -> rusqlite::Result<Vec<GenerationMeta>> {
    let mut stmt = conn.prepare(
        "SELECT generation_id, generation_number, state, created_at FROM generation \
         WHERE worktree_id = ?1 \
         ORDER BY generation_number",
    )?;
    let rows = stmt
        .query_map(params![worktree_id], |r| {
            let generation_id: String = r.get(0)?;
            let generation_number: i64 = r.get(1)?;
            let raw_state: String = r.get(2)?;
            let created_at: i64 = r.get(3)?;
            let state = GenerationState::from_db(&raw_state).ok_or_else(|| {
                Error::FromSqlConversionFailure(
                    2,
                    Type::Text,
                    format!("invalid generation.state {raw_state:?}").into(),
                )
            })?;
            Ok(GenerationMeta {
                generation_id,
                generation_number,
                state,
                created_at,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Compute the pinned generation roots for `worktree_id`: load its generations and
/// delegate to the pure [`mark_pins`] (spec 06 §5).
///
/// Per-worktree by construction (the "last `K`" counts within a worktree); the
/// store-wide union that the batched sweep walks is T06-02.
pub fn pinned_generation_roots(
    conn: &Connection,
    worktree_id: &str,
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
) -> rusqlite::Result<PinRoots> {
    let gens = generation_meta_for_worktree(conn, worktree_id)?;
    Ok(mark_pins(&gens, params, external, now_ms))
}

// ---------------------------------------------------------------------------
// Sweep phase (T06-02) — the batched, mutating half of retention/GC.
// ---------------------------------------------------------------------------

/// The bounded state-transaction size for the sweep (spec 03 §3 `[SPEC]`: "≤ 500
/// rows/tx"). Every batch deletes at most this many rows in one committed
/// transaction, so a large sweep never grows the WAL without bound or starves
/// other producers on the single write queue.
pub const SWEEP_BATCH_ROWS: usize = 500;

/// Per-table row counts for one sweep — either the rows a real sweep **deleted**
/// ([`run_sweep`]) or the rows a dry run **would** delete ([`plan_sweep`],
/// [`SweepPlan::would_delete`]).
///
/// Fields follow the delete order (spec 06 §5): the generation-scoped rows first
/// (`edges` … `generations`), then the content-graph rows that become
/// unreferenced once those generations are gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// `resolved_graph_edge` rows of candidate generations.
    pub edges: u64,
    /// `generation_unit_occurrence` rows of candidate generations.
    pub occurrences: u64,
    /// `generation_file` membership rows of candidate generations.
    pub generation_files: u64,
    /// `skipped_file` rows of candidate generations.
    pub skipped_files: u64,
    /// `generation` rows swept (the candidate generations themselves).
    pub generations: u64,
    /// `unresolved_reference` rows of orphaned file revisions.
    pub unresolved_references: u64,
    /// `parsed_unit` rows of orphaned file revisions.
    pub parsed_units: u64,
    /// `file_revision` rows that became unreferenced.
    pub file_revisions: u64,
    /// `content_blob` rows no longer referenced by any surviving `parsed_unit`.
    pub content_blobs: u64,
}

impl SweepReport {
    /// The total number of rows across every table.
    pub fn total(&self) -> u64 {
        self.edges
            + self.occurrences
            + self.generation_files
            + self.skipped_files
            + self.generations
            + self.unresolved_references
            + self.parsed_units
            + self.file_revisions
            + self.content_blobs
    }

    /// Whether the sweep touched (or would touch) no rows at all.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// The result of a dry run ([`plan_sweep`]): the generations that would be swept
/// and the per-table row counts that would be deleted, computed without mutating
/// a single canonical row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// Candidate generation ids (sorted): the `retiring`/`failed` generations not
    /// pinned by any worktree's roots or an external reference.
    pub candidate_generations: Vec<String>,
    /// Per-table counts of the rows a real sweep would delete.
    pub would_delete: SweepReport,
}

/// A failure from [`run_sweep`].
#[derive(Debug)]
#[non_exhaustive]
pub enum SweepError {
    /// A batch (or the scratch setup/teardown) transaction failed and rolled
    /// back; the store is unchanged for that batch. Earlier committed batches
    /// stand — re-running the sweep resumes them idempotently.
    Write(WriteError),
    /// A between-batch failpoint fired (tests only; requires the `failpoints`
    /// feature). Some batches committed before it; re-running completes the sweep.
    #[cfg(feature = "failpoints")]
    Interrupted,
}

impl std::fmt::Display for SweepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SweepError::Write(e) => write!(f, "sweep transaction failed: {e}"),
            #[cfg(feature = "failpoints")]
            SweepError::Interrupted => {
                write!(f, "sweep interrupted at a between-batch failpoint")
            }
        }
    }
}

impl std::error::Error for SweepError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SweepError::Write(e) => Some(e),
            #[cfg(feature = "failpoints")]
            SweepError::Interrupted => None,
        }
    }
}

/// Create the connection-local scratch tables and materialize the sweep sets.
///
/// The scratch tables live in the `temp` schema (per-connection, never part of
/// canonical `state.sqlite`). Both [`run_sweep`] and the [`plan_sweep`] dry run
/// create them on the single writer connection, confining every mutation to the
/// `temp` schema — no canonical row and no main-database WAL frame is written, so
/// a dry run satisfies "mutate nothing" (a read-only connection is `query_only`
/// and could not create them):
///
/// - `sweep_pinned` — the store-wide union of every worktree's pinned generation
///   roots (spec 06 §5, computed in Rust via [`pinned_generation_roots`]);
/// - `sweep_pinned_rev` — file revisions pinned directly by an external reference;
/// - `sweep_candidates` — `retiring`/`failed` generations **not** pinned (the only
///   GC-eligible states, spec 04 §1; the state guard also protects a concurrently
///   built `building`/`active` generation from a stale pin snapshot);
/// - `sweep_orphan_rev` — file revisions referenced only by candidate generations
///   (or by none) and not externally pinned: the reachability closure that keeps a
///   content-shared revision alive until its last referencing generation is swept.
///
/// `DROP … IF EXISTS` first so re-running on a still-open connection (e.g. after a
/// between-batch failpoint) recomputes the sets from the current live state.
fn setup_scratch(
    conn: &Connection,
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.sweep_orphan_rev;\
         DROP TABLE IF EXISTS temp.sweep_candidates;\
         DROP TABLE IF EXISTS temp.sweep_pinned_rev;\
         DROP TABLE IF EXISTS temp.sweep_pinned;\
         CREATE TEMP TABLE sweep_pinned (generation_id TEXT PRIMARY KEY);\
         CREATE TEMP TABLE sweep_pinned_rev (file_revision_id TEXT PRIMARY KEY);\
         CREATE TEMP TABLE sweep_candidates (generation_id TEXT PRIMARY KEY);\
         CREATE TEMP TABLE sweep_orphan_rev (file_revision_id TEXT PRIMARY KEY);",
    )?;

    let pinned = store_wide_pinned(conn, params, external, now_ms)?;
    {
        let mut stmt = conn.prepare("INSERT INTO sweep_pinned (generation_id) VALUES (?1)")?;
        for id in &pinned {
            stmt.execute(params![id])?;
        }
    }
    {
        let mut stmt =
            conn.prepare("INSERT INTO sweep_pinned_rev (file_revision_id) VALUES (?1)")?;
        for id in &external.referenced_file_revisions {
            stmt.execute(params![id])?;
        }
    }

    // GC candidates: retiring/failed generations that no root pinned. Materialized
    // once; deletions below leave it stable (already-deleted ids simply match no
    // rows on a resume).
    conn.execute(
        "INSERT INTO sweep_candidates (generation_id) \
         SELECT generation_id FROM generation \
         WHERE state IN ('retiring','failed') \
           AND generation_id NOT IN (SELECT generation_id FROM sweep_pinned)",
        [],
    )?;
    // Reachability closure: a file_revision is orphaned when no surviving
    // (non-candidate) generation_file references it and it is not externally
    // pinned. Referencing the candidate set (not the live generation_file after
    // deletion) makes this identical for a dry run and for a partially-applied
    // resume.
    conn.execute(
        "INSERT INTO sweep_orphan_rev (file_revision_id) \
         SELECT file_revision_id FROM file_revision \
         WHERE file_revision_id NOT IN ( \
                 SELECT file_revision_id FROM generation_file \
                 WHERE generation_id NOT IN (SELECT generation_id FROM sweep_candidates)) \
           AND file_revision_id NOT IN (SELECT file_revision_id FROM sweep_pinned_rev)",
        [],
    )?;
    Ok(())
}

/// Drop the scratch tables created by [`setup_scratch`].
fn drop_scratch(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.sweep_orphan_rev;\
         DROP TABLE IF EXISTS temp.sweep_candidates;\
         DROP TABLE IF EXISTS temp.sweep_pinned_rev;\
         DROP TABLE IF EXISTS temp.sweep_pinned;",
    )
}

/// The store-wide union of pinned generation roots across every worktree.
///
/// [`pinned_generation_roots`] is per-worktree (the "last `K`" counts within a
/// worktree); the sweep is store-wide, so it unions the roots of every worktree.
/// External-reference and lease pins enter through each per-worktree call
/// (they are worktree-independent, so the union carries them).
fn store_wide_pinned(
    conn: &Connection,
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
) -> rusqlite::Result<BTreeSet<String>> {
    let worktrees = crate::registry::all_worktree_ids(conn)?;
    let mut pinned = BTreeSet::new();
    for wt in &worktrees {
        pinned.extend(pinned_generation_roots(conn, wt, params, external, now_ms)?.generations);
    }
    Ok(pinned)
}

// Each phase names a table and the predicate selecting its sweepable rows against
// the scratch sets. `COUNT` uses the predicate directly; the batched `DELETE`
// wraps it in `rowid IN (SELECT … LIMIT ?1)` — a portable ceiling that never
// depends on `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`.
struct SweepPhase {
    /// The table swept.
    table: &'static str,
    /// The `COUNT(*)`/`SELECT rowid … WHERE` predicate over the scratch sets.
    count_pred: &'static str,
    /// The `SELECT rowid … WHERE` predicate for the batched delete. Equal to
    /// `count_pred` except for `parsed_unit`, whose delete adds the leaf-first
    /// filter (see [`PHASES`]).
    delete_pred: &'static str,
}

/// The nine sweep phases in delete order (spec 06 §5). `generation_unit_occurrence`
/// is a child of both `generation_file` and `parsed_unit`, so it precedes both;
/// `resolved_graph_edge` is a child of the occurrence, so it precedes that. The
/// `parsed_unit` delete is **leaf-first**: it only removes rows that no
/// not-yet-deleted orphan unit still names as `parent_unit_id`, so each statement
/// leaves the self-referential foreign key satisfied at its conclusion even when a
/// nested unit tree spans several batches. Its `count_pred` counts the whole
/// orphaned set (all waves) so a dry run matches the eventual total.
const PHASES: [SweepPhase; 9] = [
    SweepPhase {
        table: "resolved_graph_edge",
        count_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
        delete_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
    },
    SweepPhase {
        table: "generation_unit_occurrence",
        count_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
        delete_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
    },
    SweepPhase {
        table: "generation_file",
        count_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
        delete_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
    },
    SweepPhase {
        table: "skipped_file",
        count_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
        delete_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
    },
    SweepPhase {
        table: "generation",
        count_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
        delete_pred: "generation_id IN (SELECT generation_id FROM sweep_candidates)",
    },
    SweepPhase {
        table: "unresolved_reference",
        count_pred: "file_revision_id IN (SELECT file_revision_id FROM sweep_orphan_rev)",
        delete_pred: "file_revision_id IN (SELECT file_revision_id FROM sweep_orphan_rev)",
    },
    SweepPhase {
        table: "parsed_unit",
        count_pred: "file_revision_id IN (SELECT file_revision_id FROM sweep_orphan_rev)",
        delete_pred: "file_revision_id IN (SELECT file_revision_id FROM sweep_orphan_rev) \
             AND unit_id NOT IN ( \
                 SELECT parent_unit_id FROM parsed_unit \
                 WHERE parent_unit_id IS NOT NULL \
                   AND file_revision_id IN (SELECT file_revision_id FROM sweep_orphan_rev))",
    },
    SweepPhase {
        table: "file_revision",
        count_pred: "file_revision_id IN (SELECT file_revision_id FROM sweep_orphan_rev)",
        delete_pred: "file_revision_id IN (SELECT file_revision_id FROM sweep_orphan_rev)",
    },
    SweepPhase {
        table: "content_blob",
        count_pred: "blob_id NOT IN ( \
             SELECT blob_id FROM parsed_unit \
             WHERE file_revision_id NOT IN (SELECT file_revision_id FROM sweep_orphan_rev))",
        delete_pred: "blob_id NOT IN ( \
             SELECT blob_id FROM parsed_unit \
             WHERE file_revision_id NOT IN (SELECT file_revision_id FROM sweep_orphan_rev))",
    },
];

/// Store the per-phase count into the matching [`SweepReport`] field. The order is
/// the fixed [`PHASES`] order.
fn record(report: &mut SweepReport, phase_index: usize, rows: u64) {
    match phase_index {
        0 => report.edges = rows,
        1 => report.occurrences = rows,
        2 => report.generation_files = rows,
        3 => report.skipped_files = rows,
        4 => report.generations = rows,
        5 => report.unresolved_references = rows,
        6 => report.parsed_units = rows,
        7 => report.file_revisions = rows,
        _ => report.content_blobs = rows,
    }
}

/// Dry run: report the generations and rows a sweep **would** delete, mutating no
/// canonical row (spec 06 §5, T06-02 card "dry-run mutates nothing").
///
/// Runs as a single transaction on the writer's connection (the only connection
/// that may create the `temp` scratch tables — read-only connections are
/// `query_only`). The transaction creates the connection-local scratch tables, holds
/// the pinned/candidate/orphan sets, counts each phase against them, and drops them
/// again: the `temp` schema is never part of `state.sqlite`, so no canonical row —
/// and no main-database WAL frame — is written.
pub async fn plan_sweep(
    db: &StateDb,
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
) -> Result<SweepPlan, SweepError> {
    let (params, external) = (*params, external.clone());
    db.writer()
        .transaction(move |tx| {
            setup_scratch(tx, &params, &external, now_ms)?;

            let mut candidate_generations = {
                let mut stmt = tx
                    .prepare("SELECT generation_id FROM sweep_candidates ORDER BY generation_id")?;
                stmt.query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            candidate_generations.sort();

            let mut would_delete = SweepReport::default();
            for (i, phase) in PHASES.iter().enumerate() {
                let sql = format!(
                    "SELECT COUNT(*) FROM {} WHERE {}",
                    phase.table, phase.count_pred
                );
                let rows: i64 = tx.query_row(&sql, [], |r| r.get(0))?;
                record(&mut would_delete, i, rows as u64);
            }

            drop_scratch(tx)?;
            Ok(SweepPlan {
                candidate_generations,
                would_delete,
            })
        })
        .await
        .map_err(SweepError::Write)
}

/// Sweep unreferenced generations and the content graph they orphan, in batches of
/// [`SWEEP_BATCH_ROWS`] rows/tx (spec 06 §5, 03 §3).
///
/// Idempotent and resumable: each batch is its own committed transaction, and the
/// sweepable sets are recomputed from the live database on every call, so an
/// interruption between batches is healed by simply calling `run_sweep` again — no
/// separate progress checkpoint is needed. Returns the rows actually deleted.
pub async fn run_sweep(
    db: &StateDb,
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
) -> Result<SweepReport, SweepError> {
    run_sweep_with_batch(db, params, external, now_ms, SWEEP_BATCH_ROWS).await
}

/// [`run_sweep`] with an explicit batch ceiling, for tests (crash/resume, the
/// `parsed_unit` self-FK across a batch boundary) and future tuning. `run_sweep`
/// is the normal entry point; this never exceeds `batch_rows` rows per transaction.
pub async fn run_sweep_with_batch(
    db: &StateDb,
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
    batch_rows: usize,
) -> Result<SweepReport, SweepError> {
    // Materialize the scratch sets on the writer's connection so every batch below
    // (same connection) sees them; committed here, they persist across batches.
    let (setup_params, setup_external) = (*params, external.clone());
    db.writer()
        .transaction(move |tx| setup_scratch(tx, &setup_params, &setup_external, now_ms))
        .await
        .map_err(SweepError::Write)?;

    let mut report = SweepReport::default();
    for (i, phase) in PHASES.iter().enumerate() {
        let sql = format!(
            "DELETE FROM {} WHERE rowid IN (SELECT rowid FROM {} WHERE {} LIMIT ?1)",
            phase.table, phase.table, phase.delete_pred
        );
        let rows = delete_batched(db, sql, batch_rows).await?;
        record(&mut report, i, rows);
    }

    db.writer()
        .transaction(|tx| drop_scratch(tx))
        .await
        .map_err(SweepError::Write)?;
    Ok(report)
}

/// Run one table's batched delete to exhaustion, ≤ `batch_rows` rows per committed
/// transaction. Returns the total rows removed. A between-batch failpoint (tests
/// only) fires after a non-empty batch commits, modelling an interruption with real
/// partial progress on disk.
async fn delete_batched(db: &StateDb, sql: String, batch_rows: usize) -> Result<u64, SweepError> {
    let limit = batch_rows as i64;
    let mut total: u64 = 0;
    loop {
        let sql = sql.clone();
        let deleted = db
            .writer()
            .transaction(move |tx| tx.execute(&sql, params![limit]))
            .await
            .map_err(SweepError::Write)?;
        total += deleted as u64;
        if deleted == 0 {
            break;
        }
        #[cfg(feature = "failpoints")]
        {
            local_rag_test_support::fail_point!(
                "retention.sweep.between_batches",
                Err(SweepError::Interrupted)
            );
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a [`GenerationMeta`] tersely for table-driven cases.
    fn meta(id: &str, number: i64, state: GenerationState, created_at: i64) -> GenerationMeta {
        GenerationMeta {
            generation_id: id.to_string(),
            generation_number: number,
            state,
            created_at,
        }
    }

    /// Sorted `Vec` view of a pinned generation set, for readable assertions.
    fn ids(roots: &PinRoots) -> Vec<&str> {
        roots.generations.iter().map(String::as_str).collect()
    }

    /// A generous window/K that pins nothing on its own for the state-root cases.
    fn no_retention() -> RetentionParams {
        RetentionParams {
            keep_last_k: 0,
            window_ms: 0,
        }
    }

    /// active/building/projection_ready are pinned unconditionally; retiring/failed
    /// are never pinned *by state alone* (spec 06 §5, decision: failed not retained).
    #[test]
    fn state_roots_pin_active_building_and_target_only() {
        let now = 10_000;
        // Born well before the (zero-width) window so retention can never pin them;
        // only their *state* could, and it must not for retiring/failed.
        let gens = [
            meta("g-build", 1, GenerationState::Building, 0),
            meta("g-ready", 2, GenerationState::ProjectionReady, 0),
            meta("g-active", 3, GenerationState::Active, 0),
            meta("g-retiring", 4, GenerationState::Retiring, 0),
            meta("g-failed", 5, GenerationState::Failed, 0),
        ];
        // K=0, window=0 → the two GC-candidate states contribute nothing.
        let roots = mark_pins(&gens, &no_retention(), &ExternalPins::default(), now);
        assert_eq!(ids(&roots), vec!["g-active", "g-build", "g-ready"]);
    }

    /// Last-K boundary: K of 5 retiring generations keeps exactly the K highest
    /// numbers; K=0 keeps none; K≥count keeps all. Window is disabled here.
    #[test]
    fn last_k_boundary_keeps_highest_numbers() {
        let now = 1_000_000;
        // Created long before any window; only K can pin them. Numbers deliberately
        // supplied out of order to prove ordering is by number, not slice position.
        let gens = [
            meta("g3", 3, GenerationState::Retiring, 0),
            meta("g1", 1, GenerationState::Retiring, 0),
            meta("g5", 5, GenerationState::Retiring, 0),
            meta("g2", 2, GenerationState::Retiring, 0),
            meta("g4", 4, GenerationState::Retiring, 0),
        ];
        let ext = ExternalPins::default();

        let k = |keep_last_k: u32| RetentionParams {
            keep_last_k,
            window_ms: 0,
        };
        assert_eq!(
            ids(&mark_pins(&gens, &k(0), &ext, now)),
            Vec::<&str>::new(),
            "K=0 keeps none"
        );
        assert_eq!(
            ids(&mark_pins(&gens, &k(2), &ext, now)),
            vec!["g4", "g5"],
            "K=2 keeps the two highest numbers"
        );
        assert_eq!(
            ids(&mark_pins(&gens, &k(5), &ext, now)),
            vec!["g1", "g2", "g3", "g4", "g5"],
            "K=count keeps all"
        );
        assert_eq!(
            ids(&mark_pins(&gens, &k(99), &ext, now)),
            vec!["g1", "g2", "g3", "g4", "g5"],
            "K>count keeps all"
        );
    }

    /// Window-T boundary uses `created_at >= now - window_ms` (inclusive lower edge).
    #[test]
    fn window_boundary_is_inclusive_lower_edge() {
        let now = 1_000;
        let window_ms = 100; // floor = 900
        let gens = [
            meta("g-before", 1, GenerationState::Retiring, 899),
            meta("g-edge", 2, GenerationState::Retiring, 900),
            meta("g-inside", 3, GenerationState::Retiring, 950),
        ];
        let params = RetentionParams {
            keep_last_k: 0,
            window_ms,
        };
        let roots = mark_pins(&gens, &params, &ExternalPins::default(), now);
        assert_eq!(
            ids(&roots),
            vec!["g-edge", "g-inside"],
            "created_at == floor is inside; one ms earlier is out"
        );
    }

    /// A window wider than `now` saturates its floor and pins every candidate rather
    /// than underflowing.
    #[test]
    fn oversized_window_saturates_and_pins_all() {
        let now = 500;
        let params = RetentionParams {
            keep_last_k: 0,
            window_ms: i64::MAX,
        };
        let gens = [
            meta("g0", 1, GenerationState::Retiring, 0),
            meta("g1", 2, GenerationState::Retiring, 400),
        ];
        let roots = mark_pins(&gens, &params, &ExternalPins::default(), now);
        assert_eq!(ids(&roots), vec!["g0", "g1"]);
    }

    /// K and T are a **union**: a candidate outside last-K but inside the window is
    /// pinned, and one inside last-K but outside the window is pinned.
    #[test]
    fn k_and_t_are_a_union() {
        let now = 10_000;
        // g-old: highest number (in last-1) but ancient (outside window).
        // g-recent: lowest number (outside last-1) but freshly created (in window).
        let gens = [
            meta("g-old", 9, GenerationState::Retiring, 0),
            meta("g-mid", 5, GenerationState::Retiring, 0),
            meta("g-recent", 1, GenerationState::Retiring, 9_999),
        ];
        let params = RetentionParams {
            keep_last_k: 1,
            window_ms: 100, // floor = 9_900
        };
        let roots = mark_pins(&gens, &params, &ExternalPins::default(), now);
        assert_eq!(
            ids(&roots),
            vec!["g-old", "g-recent"],
            "last-K keeps g-old; window keeps g-recent; g-mid dropped by both"
        );
    }

    /// A lease pins its generation only while unexpired: `lease_until_ms > now` pins,
    /// `== now` and `< now` do not.
    #[test]
    fn only_unexpired_leases_pin() {
        let now = 1_000;
        // A retiring generation that neither K nor T would keep.
        let gens = [meta("g", 1, GenerationState::Retiring, 0)];
        let params = no_retention();

        let with_lease = |until: i64| ExternalPins {
            leases: vec![JobLease {
                generation_id: "g".to_string(),
                lease_until_ms: until,
            }],
            ..ExternalPins::default()
        };

        assert_eq!(
            ids(&mark_pins(&gens, &params, &with_lease(1_001), now)),
            vec!["g"],
            "lease in the future pins"
        );
        assert_eq!(
            ids(&mark_pins(&gens, &params, &with_lease(1_000), now)),
            Vec::<&str>::new(),
            "lease expiring exactly now does not pin"
        );
        assert_eq!(
            ids(&mark_pins(&gens, &params, &with_lease(999), now)),
            Vec::<&str>::new(),
            "expired lease does not pin"
        );
    }

    /// A lease can pin a generation that is not even in the local slice (its backing
    /// subsystem knows about it); the id still appears in the roots.
    #[test]
    fn lease_pins_generation_absent_from_slice() {
        let now = 0;
        let ext = ExternalPins {
            leases: vec![JobLease {
                generation_id: "g-elsewhere".to_string(),
                lease_until_ms: 1,
            }],
            ..ExternalPins::default()
        };
        let roots = mark_pins(&[], &no_retention(), &ext, now);
        assert_eq!(ids(&roots), vec!["g-elsewhere"]);
    }

    /// External references pin generations regardless of state — including `failed`
    /// and generations old enough that retention would otherwise drop them.
    #[test]
    fn external_references_pin_regardless_of_state() {
        let now = 1_000_000;
        let gens = [
            meta("g-failed", 1, GenerationState::Failed, 0),
            meta("g-old-retiring", 2, GenerationState::Retiring, 0),
        ];
        let ext = ExternalPins {
            referenced_generations: BTreeSet::from(["g-failed".to_string()]),
            ..ExternalPins::default()
        };
        let roots = mark_pins(&gens, &no_retention(), &ext, now);
        assert_eq!(
            ids(&roots),
            vec!["g-failed"],
            "referenced failed generation pinned; unreferenced old retiring dropped"
        );
    }

    /// `failed` generations are never pinned by the retention window, no matter how
    /// generous K/T are (regression for the decision that failed is not retained).
    #[test]
    fn failed_is_never_pinned_by_retention() {
        let now = 1_000;
        let gens = [
            meta("g-failed-new", 2, GenerationState::Failed, now),
            meta("g-failed-old", 1, GenerationState::Failed, 0),
        ];
        let params = RetentionParams {
            keep_last_k: 99,
            window_ms: i64::MAX,
        };
        let roots = mark_pins(&gens, &params, &ExternalPins::default(), now);
        assert_eq!(
            ids(&roots),
            Vec::<&str>::new(),
            "no retention parameter pins a failed generation"
        );
    }

    /// The referenced-`file_revision` set is carried through verbatim.
    #[test]
    fn referenced_file_revisions_pass_through() {
        let ext = ExternalPins {
            referenced_file_revisions: BTreeSet::from(["r1".to_string(), "r2".to_string()]),
            ..ExternalPins::default()
        };
        let roots = mark_pins(&[], &no_retention(), &ext, 0);
        assert_eq!(
            roots.referenced_file_revisions,
            BTreeSet::from(["r1".to_string(), "r2".to_string()])
        );
        assert!(roots.generations.is_empty());
    }

    /// The mark is order-independent: shuffling the input slice yields an identical
    /// pinned set (determinism requirement).
    #[test]
    fn mark_is_order_independent() {
        let now = 10_000;
        let params = RetentionParams {
            keep_last_k: 2,
            window_ms: 5_000,
        };
        let a = [
            meta("g1", 1, GenerationState::Retiring, 0),
            meta("g2", 2, GenerationState::Retiring, 6_000),
            meta("g3", 3, GenerationState::Retiring, 9_000),
            meta("g4", 4, GenerationState::Active, 0),
        ];
        let b = [a[3].clone(), a[0].clone(), a[2].clone(), a[1].clone()];
        assert_eq!(
            mark_pins(&a, &params, &ExternalPins::default(), now),
            mark_pins(&b, &params, &ExternalPins::default(), now),
        );
    }

    /// `RetentionParams::from_storage_config` maps the spec defaults: `K = 2`,
    /// `T = 168 h = 604_800_000 ms`.
    #[test]
    fn params_from_default_storage_config() {
        let params = RetentionParams::from_storage_config(&StorageConfig::default());
        assert_eq!(params.keep_last_k, 2);
        assert_eq!(params.window_ms, 604_800_000);
    }

    /// An absurd TTL cannot overflow the millisecond window.
    #[test]
    fn from_storage_config_saturates_absurd_ttl() {
        let cfg = StorageConfig {
            retired_generations_ttl_h: u64::MAX,
            ..StorageConfig::default()
        };
        let params = RetentionParams::from_storage_config(&cfg);
        assert_eq!(params.window_ms, i64::MAX);
    }

    /// The bounded transaction ceiling is the spec's `[SPEC: ≤ 500 rows/tx]`.
    #[test]
    fn sweep_batch_ceiling_is_500() {
        assert_eq!(SWEEP_BATCH_ROWS, 500);
    }

    /// [`SweepReport::total`] sums every field; [`SweepReport::is_empty`] is the
    /// zero case.
    #[test]
    fn sweep_report_total_and_is_empty() {
        assert!(SweepReport::default().is_empty());
        let r = SweepReport {
            edges: 1,
            occurrences: 2,
            generation_files: 3,
            skipped_files: 4,
            generations: 5,
            unresolved_references: 6,
            parsed_units: 7,
            file_revisions: 8,
            content_blobs: 9,
        };
        assert_eq!(r.total(), 45);
        assert!(!r.is_empty());
    }

    /// [`record`] writes each phase index into its own [`SweepReport`] field, in the
    /// fixed [`PHASES`] order — no two indices collide, so a full sweep populates a
    /// distinct field per phase.
    #[test]
    fn record_maps_each_phase_to_its_own_field() {
        assert_eq!(PHASES.len(), 9);
        let mut report = SweepReport::default();
        for i in 0..PHASES.len() {
            record(&mut report, i, (i as u64) + 1);
        }
        assert_eq!(
            report,
            SweepReport {
                edges: 1,
                occurrences: 2,
                generation_files: 3,
                skipped_files: 4,
                generations: 5,
                unresolved_references: 6,
                parsed_units: 7,
                file_revisions: 8,
                content_blobs: 9,
            }
        );
    }
}
