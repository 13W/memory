//! Expected `embedding_cache` subjects for a model space (spec 10 §3/§4) — T11-04.
//!
//! Two subsystems need the same answer to "which `embedding_cache` rows *should*
//! exist for model space M?", from opposite directions:
//!
//! * the **backfill worker** (`local_rag_embed::backfill`) embeds whatever is
//!   missing from that set — spec 10 §4 step 2;
//! * **eviction** ([`crate::eviction`]) must not throw away rows that are in it —
//!   spec 03 §4.2's pin rule.
//!
//! Computing it in two places would let the two drift into a loop where one
//! evicts what the other just wrote, so it lives here once.
//!
//! # What "expected content" means
//!
//! Spec 04 §3 says a model space is `projection_ready` when "all `required`
//! representation kinds have full coverage **for the content they are expected to
//! cover**" without defining that content; this module fixes it (`[SPEC]`) as the
//! **pin roots** (spec 06 §5), unioned across every worktree:
//! generations in `active`/`building`/`projection_ready` unconditionally, plus
//! `retiring` ones still inside the `K`/`T` retention window. That choice is what
//! keeps the three subsystems consistent — content the GC is required to keep is
//! exactly the content a switch may still need vectors for, so backfilling less
//! would leave a reachable generation unembeddable, and backfilling more would
//! spend embeddings on rows retention is about to delete.
//!
//! # Subjects, not points
//!
//! Spec 05 §4's *expected point set* is `occurrences × required kinds`; the
//! subject set is that collapsed by each kind's subject function. For `code_raw`
//! this is a real N:1 collapse — `content_blob` embeddings are "shared across
//! paths" `[FIXED]` (spec 03 §4.2), so two occurrences of one `blob_id` are one
//! subject, one vector, one cache row.
//!
//! # Kinds whose subject cannot be computed yet
//!
//! `code_context`'s serialization is `[OPEN]` (spec 09 §3 — decided by the
//! benchmark) and `memory`'s tables do not exist before group 14. They are
//! reported in [`SubjectSet::unsupported`] rather than silently skipped, and each
//! caller decides: eviction ignores them (a missing pin for a row that cannot
//! exist is harmless), while the backfill worker refuses to run, because
//! "silently zero expected" would make `Coverage::fully_covered` true for an
//! uncovered kind and promote the model space on a lie (spec 02 §6: nothing
//! degrades silently).

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use local_rag_core::identity::domain::subject_content_blob;

use crate::cache::{EmbeddingKey, SubjectKind};
use crate::code::content_blob_ids_for_generation;
use crate::registry::{
    ModelSpaceState, RepresentationKind, all_worktree_ids, default_model_space_id,
    model_space_ids_in_states, model_space_required_representation_ids, model_space_state,
    projection_state,
};
use crate::retention::{ExternalPins, RetentionParams, pinned_generation_roots};

/// The expected subject keys of one model space, plus the required kinds whose
/// subject function does not exist yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubjectSet {
    /// Every `embedding_cache` key the model space is expected to hold.
    pub keys: BTreeSet<EmbeddingKey>,
    /// `required` kinds that could not be resolved to subjects at all
    /// (`code_context`: format `[OPEN]`; `memory`: no tables before group 14).
    pub unsupported: BTreeSet<RepresentationKind>,
    /// The generations the set was computed over (the pin roots).
    pub generations: BTreeSet<String>,
}

impl SubjectSet {
    /// Expected subject count per representation kind — the `expected` half of
    /// [`CoverageEntry`](crate::registry::CoverageEntry), grouped the way
    /// coverage is (spec 10 §3).
    pub fn expected_per_kind(
        &self,
        representations: &[(RepresentationKind, String)],
    ) -> BTreeMap<RepresentationKind, u64> {
        let mut counts: BTreeMap<RepresentationKind, u64> = BTreeMap::new();
        for (kind, representation_id) in representations {
            let n = self
                .keys
                .iter()
                .filter(|k| &k.representation_id == representation_id)
                .count() as u64;
            *counts.entry(*kind).or_default() += n;
        }
        counts
    }
}

/// Generations pinned store-wide (spec 06 §5 pin roots), unioned across every
/// worktree.
///
/// The same loop [`crate::retention`]'s own crate-private `store_wide_pinned`
/// runs, exposed because the backfill worker and the eviction pin rule both need
/// it; `pinned_generation_roots` stays the single implementation of the policy.
pub fn pinned_generations(
    conn: &Connection,
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
) -> rusqlite::Result<BTreeSet<String>> {
    let mut pinned = BTreeSet::new();
    for worktree_id in all_worktree_ids(conn)? {
        pinned.extend(
            pinned_generation_roots(conn, &worktree_id, params, external, now_ms)?.generations,
        );
    }
    Ok(pinned)
}

/// The model spaces whose `embedding_cache` rows must be protected from eviction.
///
/// Three disjoint reasons a space qualifies:
///
/// * it is referenced by a worktree's projection state (`active`/`projected`/
///   `target`) — the row is serving, or is about to serve, a search;
/// * it is `building` or `projection_ready` — a backfill is producing exactly
///   these rows, and *nothing else* references them yet (a new space enters
///   `worktree_projection_state` only at switch time, spec 10 §4 step 4), so
///   without this second rule the LRU would race the worker that is filling it;
/// * it is the **default** space (`store_settings.default_model_space_id`).
///   Spec 05 §8 `[FIXED]` has every dormant or freshly-opened worktree migrate to
///   the default space at its next open, so its rows are pending work for a
///   worktree that has simply not opened yet — indistinguishable, from the
///   cache's point of view, from a space a worktree already references.
///
/// A space matching none of the three (a `retiring` one no worktree still
/// references, spec 04 §3) is deliberately unprotected: that is precisely when
/// "its cache rows become evictable" (spec 10 §4 step 6).
pub fn protected_model_space_ids(conn: &Connection) -> rusqlite::Result<BTreeSet<String>> {
    let mut ids: BTreeSet<String> = model_space_ids_in_states(
        conn,
        &[ModelSpaceState::Building, ModelSpaceState::ProjectionReady],
    )?
    .into_iter()
    .collect();

    if let Some(default_id) = default_model_space_id(conn)? {
        ids.insert(default_id);
    }

    for worktree_id in all_worktree_ids(conn)? {
        let Some(row) = projection_state(conn, &worktree_id)? else {
            continue;
        };
        for id in [
            row.active_model_space_id,
            row.projected_model_space_id,
            row.target_model_space_id,
        ]
        .into_iter()
        .flatten()
        {
            ids.insert(id);
        }
    }
    Ok(ids)
}

/// The subject keys model space `model_space_id` is expected to hold over
/// `generations`.
///
/// `generations` is supplied by the caller (normally [`pinned_generations`]) so a
/// diagnostic or a test can scope the computation without re-deriving the pin
/// policy.
pub fn expected_subject_keys(
    conn: &Connection,
    model_space_id: &str,
    generations: &BTreeSet<String>,
) -> rusqlite::Result<SubjectSet> {
    let representations = model_space_required_representation_ids(conn, model_space_id)?;
    let mut set = SubjectSet {
        generations: generations.clone(),
        ..SubjectSet::default()
    };

    for (kind, representation_id) in &representations {
        match kind {
            RepresentationKind::CodeRaw => {
                for generation_id in generations {
                    for (_occurrence_id, blob_id) in
                        content_blob_ids_for_generation(conn, generation_id)?
                    {
                        // N occurrences of one blob collapse to one subject; the
                        // BTreeSet does the dedup (spec 03 §4.2 `[FIXED]`).
                        set.keys.insert(EmbeddingKey {
                            subject_kind: SubjectKind::ContentBlob,
                            subject_hash: subject_content_blob(&blob_id),
                            representation_id: representation_id.clone(),
                        });
                    }
                }
            }
            // No subject function exists for these yet — see the module docs.
            other => {
                set.unsupported.insert(*other);
            }
        }
    }
    Ok(set)
}

/// Every protected subject key in the store: the pin set eviction must honor.
///
/// Union of [`expected_subject_keys`] over [`protected_model_space_ids`] against
/// [`pinned_generations`]. `unsupported` kinds are ignored here on purpose: a
/// kind with no subject function has no `embedding_cache` row to protect.
pub fn protected_subject_keys(
    conn: &Connection,
    params: &RetentionParams,
    external: &ExternalPins,
    now_ms: i64,
) -> rusqlite::Result<BTreeSet<EmbeddingKey>> {
    let generations = pinned_generations(conn, params, external, now_ms)?;
    let mut pinned = BTreeSet::new();
    for model_space_id in protected_model_space_ids(conn)? {
        // A referenced-but-deleted space cannot contribute; `model_space_state`
        // answers `None` for it rather than erroring.
        if model_space_state(conn, &model_space_id)?.is_none() {
            continue;
        }
        pinned.extend(expected_subject_keys(conn, &model_space_id, &generations)?.keys);
    }
    Ok(pinned)
}
