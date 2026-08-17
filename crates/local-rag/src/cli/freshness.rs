//! How fresh is a worktree's index, and is any work stuck? — X-008.
//!
//! Three commands need the same two answers and must not disagree about them:
//! `project list`/`status` (per enrolled project), `doctor` (per worktree, as a
//! health section) and `stats` (for the resolved worktree). This module owns the
//! computation; the commands only format it.
//!
//! Everything is derived from
//! [`generation_meta_for_worktree`](local_rag_store::generation_meta_for_worktree),
//! which already returns every generation of a worktree with its number, state
//! and `created_at`, ordered by number. No new store query was added for X-008.

use local_rag_core::identity::Uuid;
use local_rag_store::{GenerationMeta, GenerationState};

/// Below this, a `created_at` cannot be a real Unix-millisecond timestamp:
/// 2023-11-14, years before this project existed. D-062 wrote monotonic
/// milliseconds-since-loop-start into `generation.created_at` for every
/// generation the daemon/`watch` path built, and those values are tiny (a
/// freshly started loop stamps single digits), so this threshold separates the
/// two scales with an enormous margin either way.
const PLAUSIBLE_EPOCH_MS_FLOOR: i64 = 1_700_000_000_000;

/// The generation's creation time in Unix milliseconds, repairing the pre-D-062
/// rows on read.
///
/// A generation id is a UUIDv7, whose first 48 bits *are* the creation
/// timestamp ([`Uuid::timestamp_ms`]) — minted from the wall clock even in the
/// builds whose `created_at` column was wrong. So a row written before D-062
/// still reports honestly here, without a backfill migration.
///
/// Transitional by construction: once retention/GC (spec 06 §5) has swept every
/// generation built before D-062, `created_at` is always plausible and this
/// falls through to it. Returns `None` only if neither source is usable — an id
/// that is not a UUIDv7 at all, which would be a corrupt row rather than an old
/// one.
pub fn generation_created_ms(meta: &GenerationMeta) -> Option<i64> {
    if meta.created_at >= PLAUSIBLE_EPOCH_MS_FLOOR {
        return Some(meta.created_at);
    }
    let id: Uuid = meta.generation_id.parse().ok()?;
    if id.version() != 7 {
        return None;
    }
    let from_id = i64::try_from(id.timestamp_ms()).ok()?;
    (from_id >= PLAUSIBLE_EPOCH_MS_FLOOR).then_some(from_id)
}

/// A generation newer than the active one that never became `active`.
///
/// This is work the system performed and then dropped: the reporter's live store
/// held two such generations (#3308/#3309, 457 files each) while search kept
/// serving #3307 from six days earlier. Nothing in the product reported it,
/// which is why X-008 makes it visible and lets it fail `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckGeneration {
    pub generation_id: String,
    pub generation_number: i64,
    pub state: GenerationState,
}

/// What one worktree's generation history says about its index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexFreshness {
    /// The active generation's id/number/creation time, if it has one.
    pub active: Option<(String, i64, Option<i64>)>,
    /// Generations numbered above the active one still sitting in
    /// `projection_ready`/`building` — ordered by number, ascending.
    pub stuck_newer: Vec<StuckGeneration>,
    /// Total generations on record, `retiring`/`failed` included — context for
    /// "no active generation" (never indexed vs indexed and then broken).
    pub total: usize,
}

impl IndexFreshness {
    /// Compute from the worktree's full generation list (as
    /// `generation_meta_for_worktree` returns it).
    pub fn from_generations(generations: &[GenerationMeta]) -> Self {
        let active = generations
            .iter()
            .find(|g| g.state == GenerationState::Active)
            .map(|g| {
                (
                    g.generation_id.clone(),
                    g.generation_number,
                    generation_created_ms(g),
                )
            });

        // Only generations *newer* than the active one count as stuck: an older
        // `projection_ready` row is ordinary history (a build that lost a race
        // and was superseded), while a newer one means the freshest work is
        // built but not served.
        let active_number = active.as_ref().map(|(_, n, _)| *n);
        let stuck_newer = generations
            .iter()
            .filter(|g| match active_number {
                Some(active_number) => g.generation_number > active_number,
                // With no active generation at all, any built-but-unserved
                // generation is the same symptom.
                None => true,
            })
            .filter(|g| {
                matches!(
                    g.state,
                    GenerationState::ProjectionReady | GenerationState::Building
                )
            })
            .map(|g| StuckGeneration {
                generation_id: g.generation_id.clone(),
                generation_number: g.generation_number,
                state: g.state,
            })
            .collect();

        Self {
            active,
            stuck_newer,
            total: generations.len(),
        }
    }

    /// Whether anything here is a fault rather than a state to report.
    ///
    /// Deliberately **only** stuck generations (the owner's explicit decision
    /// for X-008). "Never indexed" / "not enrolled" stay informational, keeping
    /// `DoctorReport::is_clean`'s own reasoning intact: a worktree that has
    /// simply never been indexed is a legitimate bootstrap state, not a fault.
    pub fn has_fault(&self) -> bool {
        !self.stuck_newer.is_empty()
    }
}

/// `then_ms` rendered as an age relative to `now_ms`, e.g. `6d 4h ago`.
///
/// Coarse on purpose — two units are enough to answer "is my index current?",
/// and no date-formatting dependency is pulled in for it (D-056 already
/// declined to add `time`/`chrono` for human-readable dates, and the reason has
/// not changed). A timestamp in the future (a clock that moved backwards) reads
/// `just now` rather than rendering a negative age.
pub fn humanize_age(now_ms: i64, then_ms: i64) -> String {
    let delta_ms = now_ms.saturating_sub(then_ms);
    if delta_ms <= 0 {
        return "just now".to_string();
    }
    let seconds = delta_ms / 1000;
    let (days, hours, minutes) = (
        seconds / 86_400,
        (seconds % 86_400) / 3600,
        (seconds % 3600) / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h ago")
    } else if hours > 0 {
        format!("{hours}h {minutes}m ago")
    } else if minutes > 0 {
        format!("{minutes}m ago")
    } else {
        format!("{seconds}s ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_core::identity::uuidv7_from;

    fn meta(number: i64, state: GenerationState, created_at: i64, id_ms: u64) -> GenerationMeta {
        GenerationMeta {
            generation_id: uuidv7_from(id_ms, [0xAB; 10]).to_string(),
            generation_number: number,
            state,
            created_at,
        }
    }

    const REAL_MS: i64 = 1_786_000_000_000; // 2026-08-06

    #[test]
    fn a_plausible_created_at_is_used_as_is() {
        let g = meta(1, GenerationState::Active, REAL_MS, 999);
        assert_eq!(generation_created_ms(&g), Some(REAL_MS));
    }

    /// The D-062 repair: `created_at` holds milliseconds-since-loop-start, the
    /// UUIDv7 id still holds the true wall clock.
    #[test]
    fn a_monotonic_created_at_falls_back_to_the_uuidv7_timestamp() {
        let g = meta(1, GenerationState::Active, 69_146_899, REAL_MS as u64);
        assert_eq!(generation_created_ms(&g), Some(REAL_MS));
    }

    #[test]
    fn an_unusable_pair_reports_nothing_rather_than_guessing() {
        let mut g = meta(1, GenerationState::Active, 12, 34);
        assert_eq!(generation_created_ms(&g), None, "both sources implausible");
        g.generation_id = "not-a-uuid".to_string();
        assert_eq!(generation_created_ms(&g), None, "unparseable id");
    }

    #[test]
    fn newer_projection_ready_generations_are_stuck_older_ones_are_history() {
        let gens = vec![
            meta(1, GenerationState::Retiring, REAL_MS, 1),
            // Older-than-active and unserved: ordinary superseded history.
            meta(2, GenerationState::ProjectionReady, REAL_MS, 2),
            meta(3, GenerationState::Active, REAL_MS, 3),
            meta(4, GenerationState::ProjectionReady, REAL_MS, 4),
            meta(5, GenerationState::Building, REAL_MS, 5),
        ];
        let f = IndexFreshness::from_generations(&gens);

        assert_eq!(f.active.as_ref().map(|(_, n, _)| *n), Some(3));
        assert_eq!(
            f.stuck_newer
                .iter()
                .map(|s| s.generation_number)
                .collect::<Vec<_>>(),
            vec![4, 5],
            "only generations newer than the active one count",
        );
        assert!(f.has_fault());
        assert_eq!(f.total, 5);
    }

    #[test]
    fn a_healthy_worktree_has_an_active_generation_and_no_fault() {
        let gens = vec![
            meta(1, GenerationState::Retiring, REAL_MS, 1),
            meta(2, GenerationState::Active, REAL_MS, 2),
        ];
        let f = IndexFreshness::from_generations(&gens);
        assert!(f.stuck_newer.is_empty());
        assert!(!f.has_fault());
    }

    /// A worktree that never finished a first index is reported, not failed —
    /// the owner's decision, and what `DoctorReport::is_clean` already argues
    /// for the equivalent per-leg case.
    #[test]
    fn no_active_generation_alone_is_not_a_fault() {
        let f = IndexFreshness::from_generations(&[]);
        assert_eq!(f.active, None);
        assert!(!f.has_fault());
        assert_eq!(f.total, 0);
    }

    /// ...but a built-yet-unserved generation with no active one at all still
    /// is: that is exactly the `helix-code` shape, where a whole index was
    /// produced and never switched on.
    #[test]
    fn built_but_never_activated_is_a_fault_even_without_an_active_generation() {
        let gens = vec![meta(1, GenerationState::ProjectionReady, REAL_MS, 1)];
        let f = IndexFreshness::from_generations(&gens);
        assert_eq!(f.active, None);
        assert_eq!(f.stuck_newer.len(), 1);
        assert!(f.has_fault());
    }

    #[test]
    fn ages_render_coarsely_and_never_go_negative() {
        let now = 10_000_000_000_i64;
        assert_eq!(humanize_age(now, now - 30_000), "30s ago");
        assert_eq!(humanize_age(now, now - 5 * 60_000), "5m ago");
        assert_eq!(
            humanize_age(now, now - (3 * 3600 + 25 * 60) * 1000),
            "3h 25m ago"
        );
        assert_eq!(
            humanize_age(now, now - (6 * 86_400 + 4 * 3600) * 1000),
            "6d 4h ago"
        );
        assert_eq!(humanize_age(now, now + 60_000), "just now", "clock skew");
    }
}
