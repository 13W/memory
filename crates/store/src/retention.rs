//! Pin-root calculation — the pure **mark phase** of retention/GC for the
//! canonical source (spec 06 §5 `[FIXED]`), group 06 / T06-01.
//!
//! Retention is mark-and-sweep. This module is only the **mark** half: given a
//! worktree's generations (plus the tuning parameters and external references),
//! it computes the set of **pinned** generations — the ones a later sweep
//! ([T06-02](super)) must not delete. The batched, mutating sweep and the
//! `generation → file_revision` reachability closure it walks live in T06-02;
//! nothing here writes to the database.
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
}
