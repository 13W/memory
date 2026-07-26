//! `local-rag` hybrid code-search read path (spec 09 §1) — T09-03.
//!
//! This is the **snapshot/read-lock search skeleton**: the first thing that
//! ties together the pieces group 09 built separately —
//! `local_rag_store::lock::WorktreeLockRegistry` (T09-01, L2), the FTS view's
//! independent validator `local_rag_store::open_and_validate_fts` (group 08),
//! and `local_rag_projection::ShardManager` (T09-02, L3) — into one
//! orchestration that holds `L2.read` across the *entire* pipeline (spec 06
//! §3 `[FIXED]`: "L2.read → resolve active tuple → FTS5 → dense → RRF →
//! enrichment → release", no generation mixing between legs) and returns a
//! canonical degraded/error outcome (spec 02 §6).
//!
//! Not yet spec 09 §7's full response: the lexical leg is real as of T12-01
//! (the active-generation BM25 query with the spec's default weights, the
//! `name_pattern` prefix filter and §4's candidate depth), but the dense leg
//! still goes through the pre-T10 fake backend only
//! (`local_rag_projection::fake`, per CLAUDE.md's "no real dense backend before
//! the T10 spike" guardrail), and enrichment/RRF/`results[]` are the rest of
//! group 12's job (T12-02…T12-04). See the `pipeline` module's own docs for the
//! exact division of scope.

mod fusion;
mod pipeline;

pub use fusion::{FusedHit, RRF_K, rrf};

pub use pipeline::{
    DEFAULT_L2_READ_WAIT_BUDGET, DenseHit, NoopObserver, PipelineSnapshot, QueryEmbedError,
    QueryEmbedder, SearchEngine, SearchInfraError, SearchRequest, Stage, StageObserver,
    UnavailableEmbedder,
};

pub use local_rag_core::VERSION;
