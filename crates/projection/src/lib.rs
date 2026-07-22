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
//! Deliberately **not** here (owning cards): the `worktree_projection_state`
//! two-axis guards (T07-02); the write-ahead switch and the
//! `expected_point_ids(state.sqlite)` derivation, which pulls in
//! `local-rag-store` (T07-03); validate-on-open and rebuild (T07-04); and the
//! F1–F12 fault matrix itself (T07-05). No real dense backend or dense/model
//! SDK is coupled before T10 — this crate depends only on `local-rag-core`.

pub mod contract;
pub mod fake;
pub mod identity;

pub use contract::{
    DenseQuery, Hash32, PROJECTION_SCHEMA_VERSION, PointId, ProjectionError, ProjectionHead,
    ProjectionPoint, ProjectionStore, RepresentationKind, Result, ScoredPoint, ShardHandle,
    ShardParams,
};
pub use fake::{FakeProjectionStore, FakeShard};
pub use identity::{head, manifest_hash, projection_point_id};

#[cfg(feature = "failpoints")]
pub use fake::{Corruption, ShardInspection};

pub use local_rag_core::VERSION;
