//! Validate-on-open (spec 05 §6 `[FIXED]`) — T07-04.
//!
//! [`validate`] is the pure predicate table run on **every** shard open (daemon
//! start, LRU re-open, post-crash) before the shard may serve any search. It
//! takes already-read inputs (the `worktree_projection_state` row, the shard's
//! `ProjectionHead`, its point count, and an independently recomputed manifest
//! hash) and returns the first [`Divergence`] found, or `None` if the shard is
//! trustworthy. No I/O happens here — [`crate::rebuild::open_and_validate`]
//! gathers the inputs and drives the repair.

use local_rag_store::{ProjectionStateRow, ProjectionStatus};

use crate::contract::{Hash32, ProjectionHead};

/// Why validate-on-open judged a shard untrustworthy (spec 05 §6's predicate
/// table, in the order checked). The first predicate that fires is reported;
/// later ones are not evaluated (mirrors
/// [`local_rag_store::check_invariants`]'s single-violation style).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Divergence {
    /// `worktree_projection_state.status != 'clean'` — a switch or a previous
    /// rebuild never reached completion.
    NotClean {
        /// The status actually found.
        status: ProjectionStatus,
    },
    /// The active and projected tuples differ even though status claims clean.
    ActiveProjectedMismatch,
    /// No head was ever written, or it does not carry the last completed op's id.
    HeadMissing,
    /// A head is present but its `projection_op_id` does not match the row's.
    OpIdMismatch,
    /// The head's `(generation_id, model_space_id)` does not match the row's
    /// active tuple.
    HeadTupleMismatch,
    /// The head's claimed point count does not match the shard's actual count.
    PointCountMismatch {
        /// The count the head claims.
        head: u64,
        /// The shard's actual point count.
        shard: u64,
    },
    /// Counts agree but the recomputed manifest hash does not match the head's
    /// — the strong check: catches an identical count with a differing ID set
    /// (spec 05 §10 F8) that a bare count comparison would miss.
    ManifestMismatch,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Divergence::NotClean { status } => {
                write!(f, "status is {:?}, not clean", status.as_str())
            }
            Divergence::ActiveProjectedMismatch => {
                write!(f, "active tuple does not match projected tuple")
            }
            Divergence::HeadMissing => write!(f, "shard has no head"),
            Divergence::OpIdMismatch => write!(f, "head op id does not match the recorded op id"),
            Divergence::HeadTupleMismatch => {
                write!(f, "head tuple does not match the active tuple")
            }
            Divergence::PointCountMismatch { head, shard } => {
                write!(f, "head claims {head} points, shard actually has {shard}")
            }
            Divergence::ManifestMismatch => {
                write!(f, "shard point set does not match head manifest")
            }
        }
    }
}

/// The first two predicates of [`validate`]'s table, decidable from
/// `worktree_projection_state` alone, before any shard I/O (spec 05 §6).
/// Extracted so a caller that must not open the shard when it isn't already
/// known to exist ([`crate::rebuild::check_dense`], T16-03) can run these
/// first: `ProjectionStore::open`'s own implementations (e.g.
/// `BruteForceShard::open`) create the shard directory as a side effect if it
/// is missing, so a caller with nothing to gain from opening a shard that a
/// row-only divergence already condemns should never reach that call.
pub fn validate_row_only(row: &ProjectionStateRow) -> Option<Divergence> {
    if row.status != ProjectionStatus::Clean {
        return Some(Divergence::NotClean { status: row.status });
    }
    if row.active_generation_id != row.projected_generation_id
        || row.active_model_space_id != row.projected_model_space_id
    {
        return Some(Divergence::ActiveProjectedMismatch);
    }
    None
}

/// Run spec 05 §6's predicate table against already-read inputs.
///
/// `shard_manifest_hash` MUST be recomputed from the shard's *actual*
/// `point_ids()` under the head's own claimed tuple (`identity::manifest_hash`)
/// — a pure self-consistency check, independent of [`Divergence::HeadTupleMismatch`]
/// (which instead checks the head's tuple against `state.sqlite`'s active tuple).
/// When `head` is `None` this value is never inspected (the function returns at
/// [`Divergence::HeadMissing`] first) — callers that have no head to hash
/// against may pass any placeholder.
pub fn validate(
    row: &ProjectionStateRow,
    head: Option<&ProjectionHead>,
    shard_point_count: u64,
    shard_manifest_hash: &Hash32,
) -> Option<Divergence> {
    if let Some(divergence) = validate_row_only(row) {
        return Some(divergence);
    }
    let Some(head) = head else {
        return Some(Divergence::HeadMissing);
    };
    if row.projection_op_id.as_deref() != Some(head.projection_op_id.to_string()).as_deref() {
        return Some(Divergence::OpIdMismatch);
    }
    if row.active_generation_id.as_deref() != Some(head.generation_id.to_string()).as_deref()
        || row.active_model_space_id.as_deref() != Some(head.model_space_id.to_string()).as_deref()
    {
        return Some(Divergence::HeadTupleMismatch);
    }
    if head.point_count != shard_point_count {
        return Some(Divergence::PointCountMismatch {
            head: head.point_count,
            shard: shard_point_count,
        });
    }
    if head.manifest_hash != *shard_manifest_hash {
        return Some(Divergence::ManifestMismatch);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_core::identity::Uuid;

    fn wt() -> Uuid {
        "01234567-89ab-7122-b344-5566778899aa".parse().unwrap()
    }
    fn gen_id() -> Uuid {
        "0000000a-0000-7000-8000-00000000000b".parse().unwrap()
    }
    fn other_gen_id() -> Uuid {
        "0000000a-0000-7000-8000-00000000000c".parse().unwrap()
    }
    fn ms() -> Uuid {
        "0000000c-0000-7000-8000-00000000000d".parse().unwrap()
    }
    fn op() -> Uuid {
        "0000000e-0000-7000-8000-00000000000f".parse().unwrap()
    }
    fn other_op() -> Uuid {
        "0000000e-0000-7000-8000-000000000010".parse().unwrap()
    }

    /// A fully consistent `clean` row + head + shard, satisfying every predicate.
    fn valid_row() -> ProjectionStateRow {
        ProjectionStateRow {
            worktree_id: wt().to_string(),
            active_generation_id: Some(gen_id().to_string()),
            active_model_space_id: Some(ms().to_string()),
            projected_generation_id: Some(gen_id().to_string()),
            projected_model_space_id: Some(ms().to_string()),
            target_generation_id: None,
            target_model_space_id: None,
            projection_op_id: Some(op().to_string()),
            projection_schema_version: 1,
            status: ProjectionStatus::Clean,
            last_error: None,
            updated_at: 0,
        }
    }

    fn valid_head() -> ProjectionHead {
        crate::identity::head(wt(), gen_id(), ms(), op(), &[])
    }

    fn manifest_for(head: &ProjectionHead) -> Hash32 {
        crate::identity::manifest_hash(
            &head.worktree_id,
            &head.generation_id,
            &head.model_space_id,
            &[],
        )
    }

    #[test]
    fn fully_consistent_state_is_valid() {
        let row = valid_row();
        let head = valid_head();
        let manifest = manifest_for(&head);
        assert_eq!(
            validate(&row, Some(&head), head.point_count, &manifest),
            None
        );
    }

    #[test]
    fn not_clean_fires_alone() {
        for status in [
            ProjectionStatus::Updating,
            ProjectionStatus::Dirty,
            ProjectionStatus::Rebuilding,
        ] {
            let mut row = valid_row();
            row.status = status;
            let head = valid_head();
            let manifest = manifest_for(&head);
            assert_eq!(
                validate(&row, Some(&head), head.point_count, &manifest),
                Some(Divergence::NotClean { status }),
                "{status:?}"
            );
        }
    }

    #[test]
    fn active_projected_mismatch_fires() {
        let mut row = valid_row();
        row.projected_generation_id = Some(other_gen_id().to_string());
        let head = valid_head();
        let manifest = manifest_for(&head);
        assert_eq!(
            validate(&row, Some(&head), head.point_count, &manifest),
            Some(Divergence::ActiveProjectedMismatch)
        );
    }

    #[test]
    fn missing_head_fires() {
        let row = valid_row();
        assert_eq!(
            validate(&row, None, 0, &Hash32::from_hex("00")),
            Some(Divergence::HeadMissing)
        );
    }

    #[test]
    fn op_id_mismatch_fires() {
        let row = valid_row();
        // A head whose op id doesn't match the row's recorded op id (a stale
        // head from a previous, uncommitted-to-row op).
        let head = crate::identity::head(wt(), gen_id(), ms(), other_op(), &[]);
        let manifest = manifest_for(&head);
        assert_eq!(
            validate(&row, Some(&head), head.point_count, &manifest),
            Some(Divergence::OpIdMismatch)
        );
    }

    #[test]
    fn head_tuple_mismatch_fires() {
        let row = valid_row();
        // Head reports a different generation than the row's active tuple, but
        // (deliberately) carries the row's own op id — isolates the tuple check
        // from the op-id check.
        let head = crate::identity::head(wt(), other_gen_id(), ms(), op(), &[]);
        let manifest = manifest_for(&head);
        assert_eq!(
            validate(&row, Some(&head), head.point_count, &manifest),
            Some(Divergence::HeadTupleMismatch)
        );
    }

    #[test]
    fn point_count_mismatch_fires() {
        let row = valid_row();
        let head = valid_head();
        let manifest = manifest_for(&head);
        assert_eq!(
            validate(&row, Some(&head), head.point_count + 1, &manifest),
            Some(Divergence::PointCountMismatch {
                head: head.point_count,
                shard: head.point_count + 1,
            })
        );
    }

    /// F8: equal point count, different ID set. `PointCountMismatch` must NOT
    /// fire (counts agree); `ManifestMismatch` must (the strong check).
    #[test]
    fn manifest_mismatch_fires_on_equal_count_different_ids() {
        use crate::contract::PointId;

        let row = valid_row();
        let ids_a = [PointId::from_hex("0a"), PointId::from_hex("0b")];
        let ids_b = [PointId::from_hex("0a"), PointId::from_hex("0c")]; // same count, different set
        let head = crate::identity::head(wt(), gen_id(), ms(), op(), &ids_a);
        let shard_manifest = crate::identity::manifest_hash(&wt(), &gen_id(), &ms(), &ids_b);

        assert_eq!(head.point_count, 2);
        assert_eq!(
            validate(&row, Some(&head), 2, &shard_manifest),
            Some(Divergence::ManifestMismatch),
            "equal count must not mask a differing id set"
        );
    }

    #[test]
    fn validate_row_only_matches_validates_first_two_predicates() {
        // Clean and consistent: no row-only divergence, though a real
        // `validate` call still needs a head/shard to reach `None` overall.
        assert_eq!(validate_row_only(&valid_row()), None);

        for status in [
            ProjectionStatus::Updating,
            ProjectionStatus::Dirty,
            ProjectionStatus::Rebuilding,
        ] {
            let mut row = valid_row();
            row.status = status;
            assert_eq!(
                validate_row_only(&row),
                Some(Divergence::NotClean { status }),
                "{status:?}"
            );
        }

        let mut row = valid_row();
        row.projected_generation_id = Some(other_gen_id().to_string());
        assert_eq!(
            validate_row_only(&row),
            Some(Divergence::ActiveProjectedMismatch)
        );
    }

    /// Predicate order: the first violation found is reported even when a later
    /// one would also fire (status dominates everything else).
    #[test]
    fn earlier_predicate_wins_over_later_ones() {
        let mut row = valid_row();
        row.status = ProjectionStatus::Dirty;
        row.projected_generation_id = Some(other_gen_id().to_string()); // would also violate #2
        assert_eq!(
            validate(&row, None, 0, &Hash32::from_hex("00")), // and #3 (missing head)
            Some(Divergence::NotClean {
                status: ProjectionStatus::Dirty
            })
        );
    }
}
