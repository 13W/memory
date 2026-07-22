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
//!   through the caller-supplied [`switch::VectorSource`] seam (standing in for
//!   the not-yet-built `embedding_cache`, T11-02).
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
//! Deliberately **not** here (owning cards): the F1–F12 fault matrix itself
//! (T07-05); the real per-worktree write lock (T09-01); the
//! representation/model-space registry and the real `embedding_cache`
//! (T11-01/T11-02). No real dense backend or dense/model SDK is coupled before
//! T10.

pub mod contract;
pub mod expected;
pub mod fake;
pub mod identity;
pub mod rebuild;
pub mod switch;
pub mod validate;

pub use contract::{
    DenseQuery, Hash32, PROJECTION_SCHEMA_VERSION, PointId, ProjectionError, ProjectionHead,
    ProjectionPoint, ProjectionStore, RepresentationKind, Result, ScoredPoint, ShardHandle,
    ShardParams,
};
pub use expected::{
    ExpectedPoint, REQUIRED_REPRESENTATION_KINDS, expected_point_ids, expected_points,
};
pub use fake::{FakeProjectionStore, FakeShard};
pub use identity::{head, manifest_hash, projection_point_id};
pub use rebuild::{
    OpenOutcome, QUARANTINE_RETENTION, RebuildCause, RebuildError, RebuildOutcome,
    open_and_validate,
};
pub use switch::{SwitchCommitError, SwitchError, SwitchOutcome, VectorSource, switch};
pub use validate::{Divergence, validate};

#[cfg(feature = "failpoints")]
pub use fake::{Corruption, ShardInspection};

pub use local_rag_core::VERSION;
