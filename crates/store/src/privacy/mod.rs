//! `inspect`/`export`/`purge` — the security/privacy operator surface (spec
//! 12 §3, 08 §3, 11 §6, group 16, T16-02).
//!
//! # Scope and module placement
//!
//! This is a cross-domain module: `inspect`/`export`/`purge` all read or
//! mutate across `observation_envelope` (this crate's [`crate::observation`]),
//! `memory_entry`/`memory_evidence`/`audit_event` ([`crate::memory`]), and
//! `generation` ([`crate::registry`]). The placement rule this crate already
//! follows for [`crate::retention`]/[`crate::housekeeping`] applies here too:
//! a **single-table** "read the full row by id" getter lives in the module
//! that owns that table (`observation::observation_envelope_row`,
//! `memory::entry::memory_entry_by_id`, `registry::generation::generation_row`
//! — none of them live here), while a **cross-table composition** with no
//! single owning table (an entry joined with its evidence and audit trail; a
//! purge that touches four tables in one transaction) lives in this module.
//!
//! # Tombstone semantics (`purge_memory`, spec 12 §3 "rewrites audit
//! references to non-sensitive tombstones")
//!
//! `purge_memory` does two things to `audit_event`, not one:
//!
//! 1. Every existing `audit_event` row for the purged `memory_id` has its
//!    `payload` set to `NULL` — a prior `apply_merge`/`apply_edit` payload can
//!    carry a JSON diff that itself echoes the entry's text, and that is
//!    exactly the "non-sensitive" half of the requirement.
//! 2. A **new** trailing `audit_event` row is inserted with `op = "purge"`,
//!    `payload = NULL`, at `entity_version = <the row's version at the moment
//!    of purge> + 1`.
//!
//! The row-absence itself (`memory_entry` has no row for this id, but
//! `audit_event` still does) is already an unambiguous signal that the entity
//! was purged, for any `entity_id`, without needing to interpret `payload` at
//! all — so a dedicated `tombstoned_at` column/flag would be redundant with
//! information the schema already expresses structurally, and was not added.
//! The explicit `op = "purge"` row exists anyway (rather than relying on
//! absence alone) because every other mutation over `memory_entry` in this
//! crate (create/reinforce/resolve/retract/supersede/edit/merge) already
//! writes an audit event — purge being the one silent exception would be
//! inconsistent with a card literally about audit tombstones, and would force
//! a reader to *infer* "purged" instead of *reading* a terminal marker.
//! `audit_event.entity_kind`/`.entity_id` carry no FK (`crate::memory::audit`'s
//! own module doc), so a row outliving its `memory_entry` parent is not a
//! constraint violation.
//!
//! # Derived text is purged with its entry (T21-07, ADR-0010)
//!
//! Since migration 14 an entry may also have a `memory_text_normalization` row
//! holding an English variant of its text — a *second copy of the user's own
//! writing*, produced by a local model. `purge_memory_rows` deletes it
//! explicitly, and counts it
//! ([`PurgeMemoryReport::normalization_rows_removed`]), rather than letting the
//! table's `ON DELETE CASCADE` take it silently: a cascade cannot be reported,
//! and a count derived from "the cascade must have fired" would be a guess
//! about a pragma rather than an observation. The cascade remains as the safety
//! net for any delete path that does not come through this module.
//!
//! The same rule is why [`crate::memory::apply_edit`] drops the row when — and
//! only when — the text actually changes: a translation of text the user has
//! since replaced is exactly the kind of derived data that must not outlive its
//! source. [`inspect_memory`]/[`export_scope`] carry the row (translation and
//! provenance both), because an export exists to show everything the store
//! holds, and the original `entry.text` is already in that output.
//!
//! # `purge --all` is one transaction, not batched
//!
//! Unlike [`crate::retention`]'s sweep (which batches on purpose — partial
//! progress is fine, even desirable, for a background hygiene job with bounded
//! lock duration), `purge_all` runs the whole operation in the caller's single
//! transaction. A partially-completed purge is a **worse** outcome than a slow
//! one: the operator asked for an all-or-nothing privacy/legal action, and
//! "half the requested data survived a crash mid-purge" is not an acceptable
//! silent state to leave a store in. Atomicity here is a correctness
//! requirement, not an optimization being skipped.
//!
//! # Known, accepted limitations
//!
//! - `purge_session` can leave a `memory_entry`/`pending_memory_candidate`
//!   with zero `memory_evidence`/`candidate_evidence` rows if the purged
//!   session was its only evidence source. This is not a new class of
//!   "orphan" — `propose_candidate` already accepts an empty evidence slice
//!   today, and no invariant anywhere in this schema requires a memory entry
//!   to carry at least one evidence row. `run_candidate_expiry_sweep`
//!   eventually reclaims a candidate nobody reviews.
//! - `purge_session` never touches `audit_event`: this crate's `entity_kind`
//!   values are only `'memory_entry'`/`'candidate'` (never
//!   `'observation_envelope'`), and the only writer of `audit_event.payload`
//!   (`apply_merge`) stores a JSON array of loser memory-ids, never raw
//!   observation/session content — there is structurally nothing to
//!   tombstone there.
//! - `purge` never touches `cache.sqlite`, so the purged entry's **embedding**
//!   survives in `embedding_cache` until LRU eviction or a full cache rebuild:
//!   a writable cross-database transaction is forbidden (spec 03 §1.4), and no
//!   sweep collects a cache row merely because its subject stopped existing
//!   (`local_rag_embed::backfill` deletes only rows that fail their integrity
//!   check). Registered as **D-074** rather than left implicit — spec 12 §3
//!   calls purge the *only* hard-delete path, and a derived vector of the
//!   user's own text outliving it is a gap, not a design choice. It predates
//!   group 21 and applies to every memory entry, normalized or not.
//! - Retention's `ExternalPins.referenced_generations` ([`crate::retention`])
//!   is not wired by this module: no column on `memory_entry` or
//!   `observation_envelope` carries a real `generation_id` reference today
//!   (`valid_from_tree`/`last_verified_tree` are written by no caller), so
//!   there is nothing yet for a memory-evidence/audit/export pin to name.

mod export;
mod inspect;
mod purge;

pub use export::export_scope;
pub use inspect::{
    EvidenceSummary, MemoryInspection, ObservationInspection, inspect_generation, inspect_memory,
    inspect_observation,
};
pub use purge::{
    PurgeAllPreview, PurgeAllReport, PurgeMemoryError, PurgeMemoryPreview, PurgeMemoryReport,
    PurgeSessionPreview, PurgeSessionReport, preview_purge_all, preview_purge_memory,
    preview_purge_session, purge_all, purge_memory, purge_session,
};
