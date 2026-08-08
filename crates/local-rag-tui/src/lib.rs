//! Library half of `local-rag-tui`: screen data/compute/render transforms shared between the
//! binary ([`main`](../main/index.html)) and its tests (mirrors `local-rag-hook`'s own lib/bin
//! split — the same shape, adopted here for the same reason: `tests/*.rs` integration tests only
//! see a package's library target, never its binary target, and T18-02's live-subprocess test
//! must call this crate's own screen-compute code directly against a real `local-rag serve`).
//!
//! [`status`] is T18-02's deliverable — the Status screen (spec 11 §7): daemon identity/mode
//! (best-effort against `store.lock`, live-probed via `local_rag::daemon::fetch_welcome` when the
//! lock says ready) plus durable counts read directly from `state.sqlite`. [`repositories`] is
//! T18-03's — the Repositories screen: browse registered repositories, drill into a repository's
//! worktrees, then into one worktree's own detail. [`memory`] is T18-04's read paths plus T18-05's
//! mutation actions (approve/reject/edit/retract/merge) — the Memory screen: browse memory
//! entries/candidates with kind/state/scope filters and pagination, drill into an entry's own
//! detail + evidence, mutate through the same primitives `cli/memory.rs` uses. [`store_read`] is a
//! T18-04 extraction — the offline-safe `state.sqlite` read dance shared by every screen above,
//! moved out once a third screen needed it. [`store_write`] is T18-05's write-side counterpart —
//! the same offline-safe precaution, returning a write-capable `StateDb`, reused as-is by
//! [`repo_settings`] (T18-06). [`repo_settings`] is T18-06's — the Repo Settings screen: a
//! `data_policy` form (4 fixed values, cycled and applied immediately, no confirm-modal — the
//! backend has no MCP catalog entry to gate against) plus a generic `(key, value)` list, over
//! `crates/store/src/registry/settings.rs` — the first production caller of that primitive
//! anywhere in the workspace. `rt` (crate-internal, not re-exported) is T18-05's single-shot tokio
//! runtime for driving a mutation's `StateWriter::transaction` from this crate's otherwise fully
//! synchronous event loop, reused by every write screen that touches `state.sqlite`.
//! [`server_settings`] is T18-07's — the Server Settings screen: a staged, `Ctrl+S`-flushed form
//! over all six `local_rag_core::config::Config` sections, keyed off `config_dir` rather than
//! `StoreLayout` (a different resolver — this is the first screen not backed by `state.sqlite` at
//! all, so it does not use [`store_write`]/`rt`; `Config::save` is a plain synchronous file write).
//! `keys` (crate-internal, not re-exported) is T18-07's extraction of `step`/`is_ctrl` — identical
//! or near-identical logic that had accumulated independently in [`repositories`], [`memory`],
//! and [`repo_settings`] by the time a third occurrence appeared, this crate's own threshold for
//! sharing rather than duplicating a small helper. [`admin_client`] is T18-09's — a long-lived
//! async UDS client polling the daemon's `admin/tail_calls`/`admin/tool_stats` on a background OS
//! thread, publishing snapshots over a channel; `pub`, unlike `rt`/`keys`, because
//! `tests/logs_live.rs` (a separate compilation unit) drives it directly against a real daemon.
//! [`logs`] is T18-09's own screen — the Logs screen: recent per-call telemetry (newest first)
//! plus per-tool aggregates, or an explicit "daemon not running" stub, backed entirely by
//! [`admin_client::AdminPoller`].

pub mod admin_client;
mod keys;
pub mod logs;
pub mod memory;
pub mod repo_settings;
pub mod repositories;
mod rt;
pub mod server_settings;
pub mod status;
pub mod store_read;
pub mod store_write;
