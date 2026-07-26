//! `local-rag` projection layer (per-worktree dense shard + FTS view).
//!
//! The dense projection is always an **untrusted cache** (spec 05, principle):
//! correctness comes from a [`ProjectionHead`] written strictly last plus
//! validate-on-open, never from a durability barrier owed by the backend.
//!
//! T07-01 introduces the **backend-neutral contract and the persistent fake
//! backend** — the foundation for proving crash correctness *before* a real
//! dense backend is chosen at the T10/roadmap-step-11 spike:
//!
//! - [`contract`] — the `[FIXED]` abstraction from spec 05 §1: the
//!   [`ProjectionStore`]/[`ShardHandle`] traits and their `[SPEC]` types
//!   ([`ProjectionPoint`], [`ProjectionHead`], [`PointId`], [`Hash32`],
//!   [`DenseQuery`], [`ScoredPoint`], [`ShardParams`], [`RepresentationKind`],
//!   [`ProjectionError`]).
//! - [`identity`] — the deterministic, `state.sqlite`-free identity functions
//!   [`projection_point_id`], [`manifest_hash`] and [`head`] (spec 05 §3/§4,
//!   03 §1.2), built on `local_rag_core::identity::domain`.
//! - [`fake`] — the persistent [`FakeProjectionStore`], a real working backend
//!   for groups 08–09 that stores each shard as two atomically-written `std`
//!   files. Under the `failpoints` feature it also carries the fault-injection
//!   controls the F1–F12 matrix needs (spec 05 §10, authored in T07-05).
//!
//! T07-03 adds the **write-ahead switch** (spec 05 §5), the first module here to
//! depend on `local-rag-store`:
//!
//! - [`expected`] — `expected_point_ids(state.sqlite)` (spec 05 §4): the
//!   deterministic expected point set for a `(generation, model_space)` tuple,
//!   derived by reading `generation_unit_occurrence` through `local-rag-store`.
//! - [`switch`] — [`switch::switch`] drives one write-ahead → desired-set
//!   reconcile → commit cycle over a [`ProjectionStore`]/[`ShardHandle`] and
//!   `local-rag-store`'s `worktree_projection_state`/generation/worktree guards
//!   (T07-02, `registry::generation`, `registry::set_current_generation`),
//!   through the caller-supplied [`switch::VectorSource`] seam (the real
//!   `embedding_cache` table now exists, T11-02, `local_rag_store::cache::embedding`;
//!   wiring a real `VectorSource` impl backed by it is T11-05's).
//!
//! T07-04 adds **validate-on-open and rebuild** (spec 05 §6/§7, plus the
//! quarantine-rotation half of §8 that D-004 deferred here):
//!
//! - [`validate`] — the pure predicate table ([`validate::validate`]) that
//!   decides, from an already-read `worktree_projection_state` row and shard
//!   head/count/manifest, whether a shard is trustworthy.
//! - [`rebuild`] — [`rebuild::open_and_validate`] runs `validate` on open and,
//!   on any divergence, repairs via [`rebuild::rebuild`]: destroy or quarantine
//!   the old shard (quarantine only when it was unopenable — spec 05 §10 F12 —
//!   kept for at most [`rebuild::QUARANTINE_RETENTION`] cycles), recreate it
//!   from scratch against the **active** tuple (never the diff-based fast path
//!   `switch` uses), through the same [`switch::VectorSource`] seam.
//!
//! T09-02 adds the **ref-counted shard LRU manager** ([`manager`], spec 02 §5
//! L3, 05 §2/§8): [`manager::ShardManager`] sits in front of
//! [`ProjectionStore::open`], bounding concurrently open shards
//! (`max_open_shards`) behind a mutex held only for the map lookup/insert/
//! evict step (spec 02 §5's L3 row, realized via `LockLevel::L3`/
//! `checked_scope_sync`, T09-01), returning ref-counted `Arc<dyn ShardHandle>`
//! handles, and routing every actual physical open/reopen through
//! [`open_and_validate`] so a corrupt or evicted-then-reopened shard
//! self-heals. Single-flight (`tokio::sync::OnceCell`) collapses concurrent
//! `acquire`s of the same worktree into one physical open; a background
//! rebuild spawned per fill (throttled to one at a time store-wide) can be
//! cancelled by [`manager::ShardManager::remove`], safe by construction
//! because `rebuild`'s three transactions are each independently committed
//! (T09-01's finding that `state.sqlite` writes physically run on a
//! dedicated OS thread is what makes cooperative task cancellation leave no
//! torn write). See the module's own docs for the full design and its
//! deliberately deferred scope (dormant-model-space migration and adoption
//! into `switch`, both T11-05/group 11 — the real model-space registry T11-05
//! needs already exists, T11-01; the reconcile driver's own adoption has no
//! dedicated task yet in the current plan). The search executor is adopted by
//! T09-03 (`local_rag_search::SearchEngine`, `crates/search`).
//!
//! Deliberately **not** here (owning cards): the F1–F12 fault matrix itself
//! (T07-05). The real `embedding_cache` (T11-02,
//! `local_rag_store::cache::embedding`) and the representation/model-space
//! registry (T11-01, `local_rag_store::registry::representation`) both live in
//! `local-rag-store` — this crate does not yet consume either (see
//! `expected`/`switch`'s own module docs: wiring both is T11-05's). No real
//! dense backend or dense/model SDK is coupled before T10.

pub mod brute_force;
pub mod contract;
pub mod expected;
pub mod fake;
pub mod identity;
pub mod manager;
pub mod model_switch;
pub mod rebuild;
pub mod switch;
pub mod validate;
pub mod vectors;

pub use brute_force::{BruteForceProjectionStore, BruteForceShard, POINTS_FORMAT_VERSION};
pub use contract::{
    DenseQuery, DistanceMetric, Hash32, PROJECTION_SCHEMA_VERSION, PointId, ProjectionError,
    ProjectionHead, ProjectionPoint, ProjectionStore, RepresentationKind, Result, ScoredPoint,
    ShardHandle, ShardParams, rank_scored, similarity,
};
pub use expected::{
    CODE_REPRESENTATION_KINDS, ExpectedError, ExpectedPoint, expected_point_ids, expected_points,
    required_code_kinds,
};
pub use fake::{FakeProjectionStore, FakeShard};
pub use identity::{head, manifest_hash, projection_point_id};
pub use manager::{AcquireError, ShardManager};
pub use model_switch::{
    ModelSwitchError, code_raw_representation_key, dormant_migration_target,
    migrate_dormant_on_open, params_for_model_space, representation_key_for, shard_dir,
    switch_model_space,
};
pub use rebuild::{
    OpenOutcome, QUARANTINE_RETENTION, RebuildCause, RebuildError, RebuildOutcome,
    open_and_validate,
};
pub use switch::{SwitchCommitError, SwitchError, SwitchOutcome, VectorSource, switch};
pub use validate::{Divergence, validate};
pub use vectors::{CacheVectorSource, projection_kind_to_store};

#[cfg(feature = "failpoints")]
pub use fake::{Corruption, ShardInspection};

pub use local_rag_core::VERSION;
