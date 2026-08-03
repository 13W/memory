//! FTS validation, degradation, and rebuild (spec 06 §4, 03 §4.3/§4.4) —
//! T08-03/D-006.
//!
//! [`validate_fts_cheap`]/[`validate_fts_strong`] are the pure predicate table
//! spec 06 §4 names, split exactly the way the spec itself splits them: cheap
//! (head-missing/generation/schema/tokenizer-version/occurrence-count) is safe
//! to run on **every search** — its signature has no manifest-hash parameter
//! at all, so the expensive recompute literally cannot be paid from a
//! per-search call site by accident. Strong additionally recomputes and
//! compares the manifest hash (`[SPEC]`: "manifest checked on open + after
//! rebuilds") and is meant only for cache-open-equivalent moments. This is the
//! same "named function per legality window" idiom [`super::super::registry::
//! generation`]/[`super::super::registry::projection_state`] already use for
//! their own guarded transitions, and matches how `local_rag_projection`'s
//! `rebuild.rs` uses separate `mark_dirty`/`begin_rebuild`/`finish_rebuild`
//! functions rather than one function parameterized by a phase enum.
//!
//! [`open_and_validate_fts`] is the orchestrator: read the worktree's active
//! generation (`registry::current_generation` — `worktree.current_generation_id`,
//! **not** `worktree_projection_state.active_generation_id`; `local_rag_projection::
//! switch::commit_switch` sets both in the same transaction to the same value,
//! so they never diverge, and reading the plain `worktree` column means FTS
//! validation has zero dependency on the two-axis projection FSM), validate,
//! and on any divergence either rebuild synchronously or defer to a background
//! job depending on [`should_rebuild_synchronously`]'s fresh occurrence-count
//! estimate.
//!
//! ## Why "rebuild" has no FSM, no quarantine, unlike the dense projection's
//!
//! `fts_projection_head` (spec 03 §4.3) carries no status/FSM column, and
//! `cache.sqlite` **is** the storage — there is no separate shard directory to
//! quarantine or destroy the way `local_rag_projection::rebuild` must. T08-02's
//! [`super::materialize_fts`] is already a single atomic transaction (delete
//! the worktree's stale rows → insert the fresh set → write head last), so
//! "rebuild" here is simply **calling it again**: a crash/error mid-rebuild
//! rolls the whole transaction back, leaving the prior valid head exactly as
//! it was, and the next validation call detects the same divergence and
//! retries — convergent by construction, with no "stuck rebuilding" state even
//! representable. Concurrent rebuild attempts (including a rare race where the
//! active generation itself changes between two overlapping callers) are safe
//! for the same reason `materialize_fts` already documents: `CacheWriter` is
//! one dedicated thread draining one bounded queue, so overlapping
//! transactions serialize into a sequence of individually-valid commits; a
//! resulting head that doesn't match the (now newer) active generation is
//! exactly what the next `validate_fts_cheap` call's generation check catches.
//!
//! ## What this task does *not* invent
//!
//! The daemon/protocol-level degraded-mode vocabulary (`degraded: "dense_only"`/
//! `"lexical_only"`, `INDEX_UNAVAILABLE`, spec 02 §6 / 09 §7) does not exist yet
//! — `crates/protocol` is still a scaffold, and wiring this into a real search
//! pipeline is group 12/15. [`FtsAvailability`]/[`requires_index_unavailable`]
//! ship only the FTS-local half of that invariant (never silently swallow "both
//! legs down"), parameterized by a plain `bool` for the dense leg so this crate
//! never needs to know how dense availability is determined.
//!
//! ## D-006: validation input must come from `cache.sqlite`, never `state.sqlite`
//!
//! [`open_and_validate_fts`] answers two genuinely different questions, and
//! conflating their data sources was a real, shipped bug (D-006): "is the
//! cache's *actual current content* trustworthy" (the count/manifest
//! predicates) versus "how expensive would a real rebuild be" (the
//! sync-vs-background decision). The first MUST be read from `cache.sqlite`
//! itself — [`super::fts::fts_doc_occurrence_count`]/
//! [`super::fts::fts_doc_occurrence_ids`] — because `code::
//! occurrence_count_for_generation`/`code::occurrence_ids_for_generation`
//! read `state.sqlite`'s expected set for the *source* generation, which is
//! immutable for that generation's whole lifetime (structural sharing) and
//! therefore **cannot change** in response to someone directly deleting or
//! tampering with `fts_doc`/`fts_occurrences` rows while leaving
//! `fts_projection_head` untouched — exactly the corruption spec 06 §4's
//! strong check exists to catch (the literal "equal occurrence count,
//! different ID set" case, a direct analogue of `local_rag_projection::
//! validate::Divergence::ManifestMismatch`). The second question legitimately
//! does want `state.sqlite`'s count (it estimates the cost of re-deriving the
//! source generation from scratch) and is read separately, only once a
//! divergence is already confirmed — never reused as the validation input.

use crate::code::occurrence_count_for_generation;
use crate::registry::current_generation;
use crate::state::{OpenError, StateDb};

use super::CacheDb;
use super::CacheOpenError;
use super::fts::{
    FtsMaterializeError, FtsMaterializeOutcome, FtsProjectionHeadRow, LEXICAL_SCHEMA_VERSION,
    TOKENIZER_VERSION, fts_doc_occurrence_count, fts_doc_occurrence_ids, fts_manifest_hash,
    materialize_fts, read_fts_projection_head,
};

/// Why the FTS view was judged untrustworthy (spec 06 §4's predicate table, in
/// the order checked). The first predicate that fires is reported; later ones
/// are not evaluated (mirrors `local_rag_projection::validate::Divergence`'s
/// single-violation style).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FtsDivergence {
    /// No `fts_projection_head` row exists for this worktree.
    HeadMissing,
    /// The head's `generation_id` does not match the worktree's active
    /// generation.
    GenerationMismatch {
        /// The generation the head claims.
        head: String,
        /// The worktree's actual active generation.
        active: String,
    },
    /// The head's `lexical_schema_version` does not match the binary constant.
    LexicalSchemaVersionMismatch {
        /// The version the head claims.
        head: i64,
        /// The version this binary produces ([`LEXICAL_SCHEMA_VERSION`]).
        binary: u32,
    },
    /// The head's `tokenizer_version` does not match the binary constant.
    TokenizerVersionMismatch {
        /// The version the head claims.
        head: i64,
        /// The version this binary produces ([`TOKENIZER_VERSION`]).
        binary: u32,
    },
    /// The head's claimed occurrence count does not match the FTS view's
    /// actual current row count (D-006: `actual` MUST be a fresh
    /// `cache::fts_doc_occurrence_count` read of `cache.sqlite`'s real
    /// `fts_doc` rows for this worktree — never `code::
    /// occurrence_count_for_generation`'s `state.sqlite`-sourced count for the
    /// source generation, which is generation-invariant and cannot detect
    /// direct corruption/deletion of cache rows).
    OccurrenceCountMismatch {
        /// The count the head claims.
        head: i64,
        /// The FTS view's actual current row count.
        actual: i64,
    },
    /// Counts agree but the recomputed manifest hash does not match the
    /// head's — the strong check: catches an identical count with a differing
    /// occurrence-id set that a bare count comparison would miss (only
    /// reachable from [`validate_fts_strong`]; [`validate_fts_cheap`] has no
    /// manifest parameter to compare). D-006: the recomputed hash MUST be
    /// derived from `cache::fts_doc_occurrence_ids`'s actual current cache
    /// content — recomputing from `state.sqlite`'s expected set instead would
    /// always reproduce the head's own (possibly-corrupted-around) value and
    /// never detect this case at all.
    ManifestMismatch,
}

impl std::fmt::Display for FtsDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FtsDivergence::HeadMissing => write!(f, "fts_head: missing"),
            FtsDivergence::GenerationMismatch { head, active } => {
                write!(f, "fts_head: generation mismatch ({head} != {active})")
            }
            FtsDivergence::LexicalSchemaVersionMismatch { head, binary } => {
                write!(
                    f,
                    "fts_head: lexical_schema_version mismatch ({head} != {binary})"
                )
            }
            FtsDivergence::TokenizerVersionMismatch { head, binary } => {
                write!(
                    f,
                    "fts_head: tokenizer_version mismatch ({head} != {binary})"
                )
            }
            FtsDivergence::OccurrenceCountMismatch { head, actual } => {
                write!(
                    f,
                    "fts_head: occurrence_count mismatch ({head} != {actual})"
                )
            }
            FtsDivergence::ManifestMismatch => {
                write!(f, "fts_head: manifest_hash mismatch")
            }
        }
    }
}

/// Spec 06 §4's cheap predicate table: head-missing → generation →
/// lexical-schema-version → tokenizer-version → occurrence-count. Safe to run
/// on every search — no I/O, and (deliberately) no manifest-hash parameter
/// exists to accidentally pay the expensive recompute here.
pub fn validate_fts_cheap(
    head: Option<&FtsProjectionHeadRow>,
    active_generation_id: &str,
    actual_occurrence_count: i64,
) -> Option<FtsDivergence> {
    let Some(head) = head else {
        return Some(FtsDivergence::HeadMissing);
    };
    if head.generation_id != active_generation_id {
        return Some(FtsDivergence::GenerationMismatch {
            head: head.generation_id.clone(),
            active: active_generation_id.to_string(),
        });
    }
    if head.lexical_schema_version != i64::from(LEXICAL_SCHEMA_VERSION) {
        return Some(FtsDivergence::LexicalSchemaVersionMismatch {
            head: head.lexical_schema_version,
            binary: LEXICAL_SCHEMA_VERSION,
        });
    }
    if head.tokenizer_version != i64::from(TOKENIZER_VERSION) {
        return Some(FtsDivergence::TokenizerVersionMismatch {
            head: head.tokenizer_version,
            binary: TOKENIZER_VERSION,
        });
    }
    if head.occurrence_count != actual_occurrence_count {
        return Some(FtsDivergence::OccurrenceCountMismatch {
            head: head.occurrence_count,
            actual: actual_occurrence_count,
        });
    }
    None
}

/// Spec 06 §4's strong predicate table: [`validate_fts_cheap`]'s checks first,
/// then — only if none of those fired — the manifest hash (recomputed by the
/// caller; no I/O happens here, mirroring `local_rag_projection::validate::
/// validate`'s "takes already-read inputs" contract). Call only at
/// cache-open-equivalent moments, never per search.
pub fn validate_fts_strong(
    head: Option<&FtsProjectionHeadRow>,
    active_generation_id: &str,
    actual_occurrence_count: i64,
    recomputed_manifest_hash: &str,
) -> Option<FtsDivergence> {
    if let Some(d) = validate_fts_cheap(head, active_generation_id, actual_occurrence_count) {
        return Some(d);
    }
    // `validate_fts_cheap` already proved `head` is `Some` when it returns
    // `None` (its first predicate is `HeadMissing`).
    let head = head.expect("validate_fts_cheap returned None only when head is Some");
    if head.manifest_hash != recomputed_manifest_hash {
        return Some(FtsDivergence::ManifestMismatch);
    }
    None
}

/// Provisional — not yet calibrated against a real rebuild-latency benchmark
/// (that calibration is T12-05's job). A placeholder proxy for spec 06 §4's
/// "<2s estimated" synchronous-rebuild criterion, not an operator-facing
/// policy knob (contrast `crate::retention`'s `[OPEN: O6]` `K`/`T`, which
/// really are tunable retention policy): the nearest precedent is
/// `local_rag_projection::rebuild::QUARANTINE_RETENTION`, an internal
/// engineering constant.
pub const FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD: u64 = 5_000;

/// Whether a divergent FTS view should be rebuilt synchronously (spec 06 §4).
/// `occurrence_count` MUST be a *fresh* read of the active generation's real
/// occurrence count (`code::occurrence_count_for_generation`) — never the
/// stale head's claimed count, which is exactly what is under suspicion.
pub fn should_rebuild_synchronously(occurrence_count: u64) -> bool {
    occurrence_count < FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD
}

/// The result of [`open_and_validate_fts`].
#[derive(Debug, Clone, PartialEq)]
pub enum FtsOpenOutcome {
    /// `worktree.current_generation_id` is `NULL` — no generation has ever
    /// been activated for this worktree (bootstrap); nothing to validate or
    /// rebuild yet, and the cache is not touched.
    NoActiveGeneration,
    /// Every checked predicate passed.
    Valid,
    /// A divergence was found and repaired synchronously.
    Rebuilt(FtsMaterializeOutcome),
    /// A divergence was found, but the fresh occurrence count exceeds
    /// [`FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD`] — no rebuild was attempted
    /// here. The caller must serve a degraded dense-only response and
    /// schedule a background rebuild (group 12/15 concern).
    DeferredBackground {
        /// Why the view was judged untrustworthy.
        divergence: FtsDivergence,
        /// The fresh occurrence count that drove the sync-vs-background
        /// decision.
        occurrence_count_estimate: u64,
    },
}

/// Why [`open_and_validate_fts`] failed at the infrastructure level (a domain
/// divergence is not an error — see [`FtsOpenOutcome`]).
#[derive(Debug)]
#[non_exhaustive]
pub enum FtsRebuildError {
    /// Opening a `state.sqlite` read connection failed.
    StateOpen(OpenError),
    /// Opening a `cache.sqlite` read connection failed.
    CacheOpen(CacheOpenError),
    /// Reading `state.sqlite` failed — the active generation, or (D-006) the
    /// rebuild-cost estimate read only after a divergence is confirmed. Never
    /// the validation input itself (that is cache-sourced; see `CacheRead`).
    StateRead(rusqlite::Error),
    /// Reading `cache.sqlite` failed — `fts_projection_head`, or (D-006) the
    /// FTS view's actual current `fts_doc` row count/occurrence-ids, which is
    /// the validation input for both cheap and strong checks.
    CacheRead(rusqlite::Error),
    /// The synchronous rebuild ([`materialize_fts`]) failed.
    Materialize(FtsMaterializeError),
}

impl std::fmt::Display for FtsRebuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FtsRebuildError::StateOpen(e) => {
                write!(f, "opening state.sqlite for FTS validation failed: {e}")
            }
            FtsRebuildError::CacheOpen(e) => {
                write!(f, "opening cache.sqlite for FTS validation failed: {e}")
            }
            FtsRebuildError::StateRead(e) => {
                write!(f, "reading state.sqlite for FTS validation failed: {e}")
            }
            FtsRebuildError::CacheRead(e) => {
                write!(f, "reading cache.sqlite for FTS validation failed: {e}")
            }
            FtsRebuildError::Materialize(e) => write!(f, "FTS rebuild failed: {e}"),
        }
    }
}

impl std::error::Error for FtsRebuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FtsRebuildError::StateOpen(e) => Some(e),
            FtsRebuildError::CacheOpen(e) => Some(e),
            FtsRebuildError::StateRead(e) | FtsRebuildError::CacheRead(e) => Some(e),
            FtsRebuildError::Materialize(e) => Some(e),
        }
    }
}

/// Which of spec 06 §4's two validation cadences to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationDepth {
    /// Head-missing/generation/schema/tokenizer/count only — safe per search.
    Cheap,
    /// Cheap checks, then an independently recomputed manifest hash. Reserved
    /// for cache-open-equivalent moments (spec 06 §4: "on open + after
    /// rebuilds").
    Strong,
}

/// The result of [`check_fts`] — the read-only half of
/// [`open_and_validate_fts`], with no repair attempted or implied.
#[derive(Debug, Clone, PartialEq)]
pub enum FtsCheckOutcome {
    /// `worktree.current_generation_id` is `NULL` — no generation has ever
    /// been activated for this worktree; nothing to validate yet.
    NoActiveGeneration,
    /// Every checked predicate passed.
    Valid,
    /// A divergence was found. Unlike [`FtsOpenOutcome::DeferredBackground`],
    /// this carries no cost estimate — "how expensive would a rebuild be" is
    /// a repair-planning question a caller asks separately, only once it has
    /// decided to act on a confirmed divergence (D-006: never mixed into the
    /// validation input itself).
    Divergent {
        /// Why the view was judged untrustworthy.
        divergence: FtsDivergence,
        /// The generation the divergence was evaluated against.
        active_generation_id: String,
    },
}

/// Read-only FTS validation for `worktree_id` (spec 06 §4) — the read+compare
/// half [`open_and_validate_fts`] itself uses before deciding whether to
/// repair. Extracted so a caller that must never mutate (`local-rag doctor`,
/// T16-03) can ask "is this valid, and why not" without risking the
/// synchronous [`materialize_fts`] call `open_and_validate_fts` makes on any
/// divergence. Takes already-open connections rather than [`StateDb`]/
/// [`CacheDb`] handles precisely so a caller can supply a cache connection
/// that was never routed through [`CacheDb::open`]'s own rebuild-on-doubt
/// policy (see [`CacheDb::open_read_only`]).
pub fn check_fts(
    state_read: &rusqlite::Connection,
    cache_read: &rusqlite::Connection,
    worktree_id: &str,
    depth: ValidationDepth,
) -> Result<FtsCheckOutcome, FtsRebuildError> {
    let Some(active_generation_id) =
        current_generation(state_read, worktree_id).map_err(FtsRebuildError::StateRead)?
    else {
        return Ok(FtsCheckOutcome::NoActiveGeneration);
    };

    // D-006: the validation input is the cache's OWN actual current content,
    // never state.sqlite's expectation for the source generation — see the
    // module docs.
    let head =
        read_fts_projection_head(cache_read, worktree_id).map_err(FtsRebuildError::CacheRead)?;
    let actual_cache_occurrence_count =
        fts_doc_occurrence_count(cache_read, worktree_id).map_err(FtsRebuildError::CacheRead)?;

    let divergence = match depth {
        ValidationDepth::Cheap => validate_fts_cheap(
            head.as_ref(),
            &active_generation_id,
            actual_cache_occurrence_count,
        ),
        ValidationDepth::Strong => {
            let ids = fts_doc_occurrence_ids(cache_read, worktree_id)
                .map_err(FtsRebuildError::CacheRead)?;
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            let manifest_hash = fts_manifest_hash(worktree_id, &active_generation_id, &refs);
            validate_fts_strong(
                head.as_ref(),
                &active_generation_id,
                actual_cache_occurrence_count,
                &manifest_hash,
            )
        }
    };

    Ok(match divergence {
        None => FtsCheckOutcome::Valid,
        Some(divergence) => FtsCheckOutcome::Divergent {
            divergence,
            active_generation_id,
        },
    })
}

/// Validate the FTS view for `worktree_id` (spec 06 §4) and repair it on any
/// divergence: [`materialize_fts`] again if the fresh occurrence count is
/// under [`FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD`], else report
/// [`FtsOpenOutcome::DeferredBackground`] without touching the cache.
pub async fn open_and_validate_fts(
    state: &StateDb,
    cache: &CacheDb,
    worktree_id: &str,
    depth: ValidationDepth,
    now_ms: i64,
) -> Result<FtsOpenOutcome, FtsRebuildError> {
    let state_read = state.open_read().map_err(FtsRebuildError::StateOpen)?;
    let cache_read = cache.open_read().map_err(FtsRebuildError::CacheOpen)?;
    let outcome = check_fts(&state_read, &cache_read, worktree_id, depth)?;
    drop(state_read);
    drop(cache_read);

    let (divergence, active_generation_id) = match outcome {
        FtsCheckOutcome::NoActiveGeneration => return Ok(FtsOpenOutcome::NoActiveGeneration),
        FtsCheckOutcome::Valid => return Ok(FtsOpenOutcome::Valid),
        FtsCheckOutcome::Divergent {
            divergence,
            active_generation_id,
        } => (divergence, active_generation_id),
    };

    // A genuinely different question from the validation input above ("how
    // expensive would re-deriving the source generation be"), read only now
    // that a divergence is confirmed — never conflated with the cache-sourced
    // validation reads (D-006).
    let rebuild_cost_estimate = {
        let read = state.open_read().map_err(FtsRebuildError::StateOpen)?;
        occurrence_count_for_generation(&read, &active_generation_id)
            .map_err(FtsRebuildError::StateRead)?
    };

    if should_rebuild_synchronously(rebuild_cost_estimate as u64) {
        let outcome = materialize_fts(state, cache, worktree_id, &active_generation_id, now_ms)
            .await
            .map_err(FtsRebuildError::Materialize)?;
        Ok(FtsOpenOutcome::Rebuilt(outcome))
    } else {
        Ok(FtsOpenOutcome::DeferredBackground {
            divergence,
            occurrence_count_estimate: rebuild_cost_estimate as u64,
        })
    }
}

/// Whether the FTS lexical leg has anything trustworthy to serve (spec 02 §6 /
/// 06 §4). `None` covers both bootstrap (no generation ever indexed) and an
/// explicit divergence deferred to background — both mean "nothing to serve
/// from this leg right now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FtsAvailability {
    /// The FTS view is trustworthy.
    Valid,
    /// Nothing trustworthy to serve; `Some` names why, `None` means bootstrap.
    Unavailable(Option<FtsDivergence>),
}

/// FTS's own half of spec 02 §6's "both legs unavailable → error, never
/// silence" rule. `dense_available` is deliberately a plain `bool`, not a
/// `local_rag_projection` type — this function must not know how dense
/// availability was determined (that combinator, and the richer
/// `INDEX_UNAVAILABLE` daemon-level error type, are group 12/15's job).
pub fn requires_index_unavailable(fts: &FtsAvailability, dense_available: bool) -> bool {
    !matches!(fts, FtsAvailability::Valid) && !dense_available
}

#[cfg(test)]
mod tests {
    use super::*;

    const WT: &str = "wt-1";
    const GEN: &str = "gen-1";
    const OTHER_GEN: &str = "gen-2";

    fn valid_head() -> FtsProjectionHeadRow {
        FtsProjectionHeadRow {
            worktree_id: WT.to_string(),
            generation_id: GEN.to_string(),
            lexical_schema_version: i64::from(LEXICAL_SCHEMA_VERSION),
            tokenizer_version: i64::from(TOKENIZER_VERSION),
            occurrence_count: 2,
            manifest_hash: fts_manifest_hash(WT, GEN, &["occ-a", "occ-b"]),
            updated_at: 1000,
        }
    }

    // ---- validate_fts_cheap / validate_fts_strong -----------------------------

    #[test]
    fn fully_consistent_state_is_valid() {
        let head = valid_head();
        assert_eq!(validate_fts_cheap(Some(&head), GEN, 2), None);
        let manifest = fts_manifest_hash(WT, GEN, &["occ-a", "occ-b"]);
        assert_eq!(validate_fts_strong(Some(&head), GEN, 2, &manifest), None);
    }

    #[test]
    fn missing_head_fires() {
        assert_eq!(
            validate_fts_cheap(None, GEN, 0),
            Some(FtsDivergence::HeadMissing)
        );
        assert_eq!(
            validate_fts_strong(None, GEN, 0, "irrelevant"),
            Some(FtsDivergence::HeadMissing)
        );
    }

    #[test]
    fn generation_mismatch_fires() {
        let head = valid_head();
        assert_eq!(
            validate_fts_cheap(Some(&head), OTHER_GEN, 2),
            Some(FtsDivergence::GenerationMismatch {
                head: GEN.to_string(),
                active: OTHER_GEN.to_string(),
            })
        );
    }

    #[test]
    fn lexical_schema_version_mismatch_fires() {
        let mut head = valid_head();
        head.lexical_schema_version = i64::from(LEXICAL_SCHEMA_VERSION) + 1;
        assert_eq!(
            validate_fts_cheap(Some(&head), GEN, 2),
            Some(FtsDivergence::LexicalSchemaVersionMismatch {
                head: i64::from(LEXICAL_SCHEMA_VERSION) + 1,
                binary: LEXICAL_SCHEMA_VERSION,
            })
        );
    }

    #[test]
    fn tokenizer_version_mismatch_fires() {
        let mut head = valid_head();
        head.tokenizer_version = i64::from(TOKENIZER_VERSION) + 1;
        assert_eq!(
            validate_fts_cheap(Some(&head), GEN, 2),
            Some(FtsDivergence::TokenizerVersionMismatch {
                head: i64::from(TOKENIZER_VERSION) + 1,
                binary: TOKENIZER_VERSION,
            })
        );
    }

    #[test]
    fn occurrence_count_mismatch_fires() {
        let head = valid_head();
        assert_eq!(
            validate_fts_cheap(Some(&head), GEN, 3),
            Some(FtsDivergence::OccurrenceCountMismatch { head: 2, actual: 3 })
        );
    }

    /// Predicate order: generation mismatch is checked before occurrence count,
    /// so a row that violates both reports the earlier one (mirrors
    /// `local_rag_projection::validate`'s `earlier_predicate_wins_over_later_ones`).
    #[test]
    fn earlier_predicate_wins_over_later_ones() {
        let head = valid_head(); // occurrence_count: 2
        assert_eq!(
            validate_fts_cheap(Some(&head), OTHER_GEN, 999), // both generation AND count wrong
            Some(FtsDivergence::GenerationMismatch {
                head: GEN.to_string(),
                active: OTHER_GEN.to_string(),
            })
        );
    }

    /// F8-equivalent: equal count, different manifest hash. `validate_fts_cheap`
    /// cannot see this at all (no manifest parameter exists); only
    /// `validate_fts_strong` catches it.
    #[test]
    fn manifest_mismatch_fires_only_under_strong_check() {
        let head = valid_head();
        assert_eq!(
            validate_fts_cheap(Some(&head), GEN, 2),
            None,
            "cheap has no manifest parameter to compare"
        );
        let different_manifest = fts_manifest_hash(WT, GEN, &["occ-a", "occ-c"]); // same count, different set
        assert_eq!(
            validate_fts_strong(Some(&head), GEN, 2, &different_manifest),
            Some(FtsDivergence::ManifestMismatch)
        );
    }

    // ---- "empty FTS invalid" (spec 06 §4 `[FIXED]`) ---------------------------

    /// A genuinely empty generation with a head that honestly claims
    /// `occurrence_count = 0` and the matching empty-set manifest is a
    /// legitimate valid state — emptiness alone is not invalid.
    #[test]
    fn empty_repo_with_matching_zero_count_head_is_valid() {
        let head = FtsProjectionHeadRow {
            worktree_id: WT.to_string(),
            generation_id: GEN.to_string(),
            lexical_schema_version: i64::from(LEXICAL_SCHEMA_VERSION),
            tokenizer_version: i64::from(TOKENIZER_VERSION),
            occurrence_count: 0,
            manifest_hash: fts_manifest_hash(WT, GEN, &[]),
            updated_at: 1000,
        };
        let manifest = fts_manifest_hash(WT, GEN, &[]);
        assert_eq!(validate_fts_strong(Some(&head), GEN, 0, &manifest), None);
    }

    /// The same empty generation, but no head was ever written. Must be
    /// `HeadMissing`, never treated as "0 == 0, therefore valid" — an absent
    /// FTS view is never silently equivalent to a legitimately empty one.
    #[test]
    fn missing_head_is_invalid_even_when_generation_is_empty() {
        assert_eq!(
            validate_fts_cheap(None, GEN, 0),
            Some(FtsDivergence::HeadMissing)
        );
    }

    // ---- diagnostics: "exact reason" -------------------------------------------

    #[test]
    fn tokenizer_version_mismatch_message_states_both_values() {
        let d = FtsDivergence::TokenizerVersionMismatch { head: 3, binary: 4 };
        assert_eq!(
            d.to_string(),
            "fts_head: tokenizer_version mismatch (3 != 4)"
        );
    }

    #[test]
    fn lexical_schema_version_mismatch_message_states_both_values() {
        let d = FtsDivergence::LexicalSchemaVersionMismatch { head: 1, binary: 2 };
        assert_eq!(
            d.to_string(),
            "fts_head: lexical_schema_version mismatch (1 != 2)"
        );
    }

    #[test]
    fn occurrence_count_mismatch_message_states_both_values() {
        let d = FtsDivergence::OccurrenceCountMismatch {
            head: 10,
            actual: 12,
        };
        assert_eq!(
            d.to_string(),
            "fts_head: occurrence_count mismatch (10 != 12)"
        );
    }

    #[test]
    fn generation_mismatch_message_states_both_values() {
        let d = FtsDivergence::GenerationMismatch {
            head: "g1".to_string(),
            active: "g2".to_string(),
        };
        assert_eq!(d.to_string(), "fts_head: generation mismatch (g1 != g2)");
    }

    // ---- rebuild-cost threshold ------------------------------------------------

    #[test]
    fn should_rebuild_synchronously_respects_threshold() {
        assert!(should_rebuild_synchronously(
            FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD - 1
        ));
        assert!(!should_rebuild_synchronously(
            FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD
        ));
        assert!(!should_rebuild_synchronously(
            FTS_SYNC_REBUILD_OCCURRENCE_THRESHOLD + 1
        ));
    }

    // ---- availability combinator ------------------------------------------------

    #[test]
    fn both_legs_unavailable_signals_index_unavailable() {
        assert!(requires_index_unavailable(
            &FtsAvailability::Unavailable(Some(FtsDivergence::HeadMissing)),
            false
        ));
    }

    #[test]
    fn fts_valid_alone_does_not_require_index_unavailable() {
        assert!(!requires_index_unavailable(&FtsAvailability::Valid, false));
    }

    #[test]
    fn dense_available_alone_does_not_require_index_unavailable() {
        assert!(!requires_index_unavailable(
            &FtsAvailability::Unavailable(Some(FtsDivergence::HeadMissing)),
            true
        ));
    }

    #[test]
    fn bootstrap_fts_with_dense_down_also_signals_index_unavailable() {
        assert!(requires_index_unavailable(
            &FtsAvailability::Unavailable(None),
            false
        ));
    }
}
