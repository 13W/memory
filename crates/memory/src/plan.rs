//! Collapsing the ops of one routed plan that speak about the same
//! `memory_entry` (`D-121`, `T23-05`).
//!
//! **The defect, measured rather than read.** Consolidation runs failed with
//! `optimistic conflict: expected entry_version V, found V+1`, were retried up
//! to eight times — each retry a full local generation — and then escalated by
//! `D-050`/`D-069`'s attempt cap into a permanent dead-letter. `D-121`
//! recorded the cause as a plan built against a version an outside writer had
//! moved. On the owner's live store all eight such rows have
//! `found = expected + 1` and an op index of at least 3; for one of them the
//! audit trail shows no entry anywhere reaching the "found" version during the
//! failure window. An outside race cannot produce that: it would show
//! arbitrary version gaps and would land on the first op as readily as the
//! sixth.
//!
//! **The real mechanism is a plan contradicting itself.**
//! [`crate::router::route`] calls [`crate::guard::materialize`] once per op,
//! and each reads its target's `entry_version` fresh — but all of them before
//! any op is applied. `guard`'s `D-078` rewrite turns a `create` of text that
//! already exists into a `Reinforce` of that entry. So a window in which the
//! model proposes one existing text twice yields two ops on one entry carrying
//! one snapshot: the first moves `V -> V+1`, the second still expects `V`.
//! Deterministic, every time. That the model does repeat itself inside one
//! window is measured too: 569 runs proposed one text more than once in a
//! single transaction, and 8 of 48 multi-op runs died this way.
//!
//! **Why the fix is here and not in `apply_run`.** Three findings, each
//! verified in the code, ruled out validating a later op against the running
//! value inside the apply transaction:
//!
//! 1. *Reading your own writes already works.* `apply_reinforce` issues its
//!    `SELECT entry_version` on the same `Transaction` the previous op wrote
//!    through, and SQLite shows an uncommitted `UPDATE` to later reads. A
//!    batch-local version map would return the number the `SELECT` already
//!    returns; to change anything it would have to *relax the comparison*,
//!    which is a different and much weaker thing.
//! 2. *A measured primary-key trap.* `memory_evidence` is keyed
//!    `(memory_id, observation_id)`, `insert_memory_evidence` is a plain
//!    `INSERT`, and `D-069`'s citation dedup works **within** one op. On the
//!    live store 70 of 96 within-run op pairs cite a shared observation, and
//!    128 of 133 ops cite exactly one — so in roughly three cases out of four
//!    the second insert would violate that key, and `classify_apply_failure`
//!    makes a constraint violation `Mechanical` on the *first* attempt. That
//!    trades a retryable failure for an immediate permanent one.
//! 3. *The version check is accidentally the only state guard there is.*
//!    `apply_reinforce` says in its own doc that it does not check the entry's
//!    `kind`/`state`. Today a batch holding `supersede E` then `reinforce E`
//!    is stopped by the version. Relaxing it would newly permit attaching
//!    evidence to an entry the same transaction had just retired.
//!
//! So the plan is folded before it leaves the router — the same "dedup at the
//! untrusted-input boundary" move `D-069` made one level down — and
//! `crates/store` is not touched at all. The optimistic check `T23-05`'s card
//! puts out of scope stays exactly as it was, for every caller.
//!
//! **What this does not fix**, stated so nobody has to rediscover it: a
//! genuine outside writer — an MCP `edit_memory`, the normalization worker,
//! another session's run — moving the version between plan and apply. That
//! remains a conflict, correctly, and the existing retry converges on it
//! because a retry re-invokes the generator and `guard` re-reads every
//! version. Convergence there is a property of re-planning, not of waiting.

use std::collections::HashSet;

use local_rag_store::{GeneratedOp, ProposedOperation};

/// How much of an entry's future one op claims.
///
/// `Ord` is the survivor rule: a statement about the entry's fate outranks a
/// statement that it was observed again. Nothing else about the ordering is
/// meaningful, and no third level is wanted — `Create`, `Noop` and every
/// `ProposeCandidate` have no target at all (see [`target_entry`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Claim {
    /// `reinforce` — adds evidence, may raise confidence, never changes text
    /// or state.
    Evidence,
    /// `supersede` / `resolve` / `retract` — each decides what becomes of the
    /// entry, and they are mutually exclusive statements about it.
    Fate,
}

/// Which `memory_entry` this op version-checks, if any.
///
/// `None` for the ops that carry no `expected_version` and therefore cannot
/// collide: `Create` and `Supersede`'s new half mint their `memory_id` inside
/// `guard`, so no other op in the same plan can name them; `Noop` writes
/// nothing; and a `ProposeCandidate` writes only `pending_memory_candidate`,
/// whose own evidence table is keyed by a freshly minted `candidate_id`.
///
/// `ProposeCandidate` is excluded for a second reason that outlives the first:
/// a candidate is a request for human review, and merging two such requests
/// would decide something a person has not decided yet — the same boundary
/// `guard` draws when it refuses to turn a review request into an automatic
/// write.
fn target_entry(op: &GeneratedOp) -> Option<&str> {
    match op {
        GeneratedOp::Materialize { operation, .. } => match operation {
            ProposedOperation::Reinforce { memory_id, .. }
            | ProposedOperation::Resolve { memory_id, .. }
            | ProposedOperation::Retract { memory_id, .. } => Some(memory_id.as_str()),
            ProposedOperation::Supersede { old_memory_id, .. } => Some(old_memory_id.as_str()),
            ProposedOperation::Create { .. } => None,
        },
        GeneratedOp::Noop | GeneratedOp::ProposeCandidate { .. } => None,
    }
}

/// The claim an op with a target makes. Only called for ops
/// [`target_entry`] answered `Some` for.
fn claim(op: &GeneratedOp) -> Claim {
    match op {
        GeneratedOp::Materialize { operation, .. } => match operation {
            ProposedOperation::Reinforce { .. } => Claim::Evidence,
            _ => Claim::Fate,
        },
        _ => Claim::Evidence,
    }
}

fn citations(op: &GeneratedOp) -> &[String] {
    match op {
        GeneratedOp::Materialize {
            evidence_observation_ids,
            ..
        }
        | GeneratedOp::ProposeCandidate {
            evidence_observation_ids,
            ..
        } => evidence_observation_ids,
        GeneratedOp::Noop => &[],
    }
}

/// Fold every group of ops naming one entry into the single op that survives
/// it, and give the survivor the group's citations.
///
/// The survivor is the op with the highest [`Claim`]; ties go to the **last**
/// in plan order, because plan order is the only evidence there is about which
/// of two contradictory statements the model made second. The survivor keeps
/// its own position, so the result is always a subsequence of the input —
/// ops are never reordered, and ops with no target are never touched.
///
/// Op indices shift, and that is immaterial: `idempotency_key` numbers the
/// already-collapsed plan, a rejected batch commits nothing for a later
/// attempt to collide with, and every retry re-invokes the generator anyway.
/// Leaving a `Noop` placeholder at a folded-away index was considered and
/// rejected — it would inflate `ApplyReport.noop` and put an op in the plan
/// the router never produced.
pub fn collapse(ops: Vec<GeneratedOp>) -> Vec<GeneratedOp> {
    // Winner per target: (target, position in `ops`). A `Vec` and not a map:
    // a plan is at most `consolidation_batch_size` ops, and insertion order is
    // the order the survivors are visited in below — which keeps the whole
    // function deterministic without sorting anything.
    let mut winner: Vec<(String, usize)> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let Some(target) = target_entry(op) else {
            continue;
        };
        let c = claim(op);
        match winner.iter_mut().find(|(t, _)| t == target) {
            // `>=` rather than `>`: a tie goes to the later op.
            Some((_, at)) if c >= claim(&ops[*at]) => *at = i,
            Some(_) => {}
            None => winner.push((target.to_string(), i)),
        }
    }
    // The citations each winner inherits, in plan order, first occurrence
    // winning — the same rule `local_rag_store`'s own `dedup_evidence_ids`
    // applies inside one op, here applied across the ops of one group. It is
    // what keeps the merged op from ever hitting `memory_evidence`'s
    // `(memory_id, observation_id)` primary key.
    let mut merged: Vec<Vec<String>> = vec![Vec::new(); ops.len()];
    // The confidence a merged `reinforce` ends up with: the last `Some` in
    // plan order, because that is exactly what `apply_reinforce`'s
    // `COALESCE(?2, confidence)` would have left after two applies.
    let mut confidence: Vec<Option<f64>> = vec![None; ops.len()];
    for op in ops.iter() {
        let Some(target) = target_entry(op) else {
            continue;
        };
        let at = winner
            .iter()
            .find(|(t, _)| t == target)
            .map(|(_, at)| *at)
            .expect("every targeted op joined a group above");
        merged[at].extend(citations(op).iter().cloned());
        if let GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce { confidence: c, .. },
            ..
        } = op
            && c.is_some()
        {
            confidence[at] = *c;
        }
    }

    let keep: HashSet<usize> = winner.iter().map(|(_, at)| *at).collect();
    ops.into_iter()
        .enumerate()
        .filter(|(i, op)| target_entry(op).is_none() || keep.contains(i))
        .map(|(i, op)| rebuild(op, std::mem::take(&mut merged[i]), confidence[i]))
        .collect()
}

/// Give a surviving op its group's citations, and — for a `reinforce` — the
/// group's confidence.
///
/// A `Fate` survivor keeps every other field untouched. When a `reinforce`
/// yields to a `supersede`, the folded citations land on the **new** entry,
/// because that is where `apply_supersede` attaches evidence. That is a
/// deliberate judgement rather than what two sequential applies would have
/// done — and it is the same one spec 08 §3 already makes for `merge`, whose
/// survivor absorbs the losers' evidence. When a `reinforce` yields to a
/// `resolve`/`retract` the question does not arise: the terminal transition
/// writes its evidence against the very same entry.
fn rebuild(
    op: GeneratedOp,
    group_citations: Vec<String>,
    group_confidence: Option<f64>,
) -> GeneratedOp {
    match op {
        GeneratedOp::Materialize {
            mut operation,
            evidence_observation_ids,
        } => {
            let cites = if group_citations.is_empty() {
                evidence_observation_ids
            } else {
                dedup_preserving_order(group_citations)
            };
            if let ProposedOperation::Reinforce { confidence, .. } = &mut operation
                && group_confidence.is_some()
            {
                *confidence = group_confidence;
            }
            GeneratedOp::Materialize {
                operation,
                evidence_observation_ids: cites,
            }
        }
        other => other,
    }
}

/// First occurrence wins, order preserved — `local_rag_store`'s
/// `dedup_evidence_ids` contract, restated here because this crate cannot
/// reach that private helper and the two must not drift.
fn dedup_preserving_order(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::with_capacity(ids.len());
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cites(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    fn reinforce(id: &str, version: i64, confidence: Option<f64>, ids: &[&str]) -> GeneratedOp {
        GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce {
                memory_id: id.to_string(),
                expected_version: version,
                confidence,
            },
            evidence_observation_ids: cites(ids),
        }
    }

    fn retract(id: &str, version: i64, ids: &[&str]) -> GeneratedOp {
        GeneratedOp::Materialize {
            operation: ProposedOperation::Retract {
                memory_id: id.to_string(),
                expected_version: version,
            },
            evidence_observation_ids: cites(ids),
        }
    }

    fn supersede(old: &str, version: i64, new: &str, ids: &[&str]) -> GeneratedOp {
        GeneratedOp::Materialize {
            operation: ProposedOperation::Supersede {
                old_memory_id: old.to_string(),
                old_expected_version: version,
                new_memory_id: new.to_string(),
                new_kind: "fact".to_string(),
                new_text: format!("text for {new}"),
                new_canonical_key: None,
                new_scope_kind: "global".to_string(),
                new_scope_owner_id: "owner".to_string(),
                new_confidence: 0.5,
                new_importance: 0.5,
                new_valid_from_tree: None,
                new_last_verified_tree: None,
            },
            evidence_observation_ids: cites(ids),
        }
    }

    fn create(id: &str, text: &str) -> GeneratedOp {
        GeneratedOp::Materialize {
            operation: ProposedOperation::Create {
                memory_id: id.to_string(),
                kind: "fact".to_string(),
                text: text.to_string(),
                canonical_key: None,
                scope_kind: "global".to_string(),
                scope_owner_id: "owner".to_string(),
                confidence: 0.5,
                importance: 0.5,
                valid_from_tree: None,
                last_verified_tree: None,
            },
            evidence_observation_ids: cites(&["o1"]),
        }
    }

    fn candidate(id: &str, target: &str) -> GeneratedOp {
        GeneratedOp::ProposeCandidate {
            candidate_id: id.to_string(),
            operation: ProposedOperation::Reinforce {
                memory_id: target.to_string(),
                expected_version: 1,
                confidence: None,
            },
            conflicts: Vec::new(),
            evidence_observation_ids: cites(&["o1"]),
        }
    }

    fn cited(op: &GeneratedOp) -> Vec<String> {
        citations(op).to_vec()
    }

    /// The live shape: `D-078` rewrites one window's two proposals of one
    /// existing text into two reinforces carrying one snapshot.
    #[test]
    fn two_reinforces_of_one_entry_become_one_carrying_both_citations() {
        let out = collapse(vec![
            reinforce("E", 3, None, &["o1", "o2"]),
            reinforce("E", 3, None, &["o2", "o3"]),
        ]);
        assert_eq!(out.len(), 1, "one entry, one op: {out:?}");
        assert_eq!(
            cited(&out[0]),
            cites(&["o1", "o2", "o3"]),
            "the union is taken in plan order, first occurrence winning — the \
             rule that keeps the merged op off `memory_evidence`'s primary key"
        );
    }

    #[test]
    fn the_merged_confidence_is_the_last_one_the_plan_stated() {
        let out = collapse(vec![
            reinforce("E", 3, Some(0.4), &["o1"]),
            reinforce("E", 3, Some(0.9), &["o2"]),
        ]);
        let GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce { confidence, .. },
            ..
        } = &out[0]
        else {
            panic!("expected a reinforce, got {out:?}");
        };
        assert_eq!(
            *confidence,
            Some(0.9),
            "`apply_reinforce` writes COALESCE(?2, confidence), so the last \
             stated value is what two applies would have left"
        );

        // And a later `None` does not erase an earlier value, for the same
        // reason: COALESCE keeps what is there.
        let out = collapse(vec![
            reinforce("E", 3, Some(0.4), &["o1"]),
            reinforce("E", 3, None, &["o2"]),
        ]);
        let GeneratedOp::Materialize {
            operation: ProposedOperation::Reinforce { confidence, .. },
            ..
        } = &out[0]
        else {
            panic!("expected a reinforce, got {out:?}");
        };
        assert_eq!(*confidence, Some(0.4));
    }

    /// A statement about the entry's fate outranks a statement that it was
    /// observed again — in either plan order.
    #[test]
    fn a_terminal_op_outranks_a_reinforce_of_the_same_entry() {
        for (label, ops) in [
            (
                "reinforce first",
                vec![reinforce("E", 3, None, &["o1"]), retract("E", 3, &["o2"])],
            ),
            (
                "retract first",
                vec![retract("E", 3, &["o2"]), reinforce("E", 3, None, &["o1"])],
            ),
        ] {
            let out = collapse(ops);
            assert_eq!(out.len(), 1, "{label}: {out:?}");
            assert!(
                matches!(
                    &out[0],
                    GeneratedOp::Materialize {
                        operation: ProposedOperation::Retract { .. },
                        ..
                    }
                ),
                "{label}: the terminal transition must survive, got {out:?}"
            );
            assert_eq!(
                cited(&out[0]).len(),
                2,
                "{label}: and it carries both citations — \
                 `apply_state_transition` attaches them to the same entry"
            );
        }
    }

    #[test]
    fn a_supersede_absorbs_a_reinforce_of_the_entry_it_retires() {
        let out = collapse(vec![
            reinforce("E", 7, Some(0.8), &["o1"]),
            supersede("E", 7, "E2", &["o2"]),
        ]);
        assert_eq!(out.len(), 1);
        let GeneratedOp::Materialize {
            operation:
                ProposedOperation::Supersede {
                    old_memory_id,
                    old_expected_version,
                    new_memory_id,
                    ..
                },
            ..
        } = &out[0]
        else {
            panic!("expected a supersede, got {out:?}");
        };
        assert_eq!((old_memory_id.as_str(), *old_expected_version), ("E", 7));
        assert_eq!(
            new_memory_id, "E2",
            "the supersede's own fields are untouched"
        );
        assert_eq!(
            cited(&out[0]),
            cites(&["o1", "o2"]),
            "the survivor absorbs the loser's evidence — spec 08 §3's own rule \
             for `merge`, applied to the same question"
        );
    }

    /// Two contradictory fates: plan order is the only evidence about which
    /// the model meant second.
    #[test]
    fn among_two_fates_the_last_one_the_plan_stated_wins() {
        let out = collapse(vec![
            supersede("E", 4, "FIRST", &["o1"]),
            supersede("E", 4, "SECOND", &["o2"]),
        ]);
        assert_eq!(out.len(), 1);
        let GeneratedOp::Materialize {
            operation: ProposedOperation::Supersede { new_memory_id, .. },
            ..
        } = &out[0]
        else {
            panic!("expected a supersede, got {out:?}");
        };
        assert_eq!(new_memory_id, "SECOND");
    }

    #[test]
    fn ops_on_different_entries_are_left_alone_in_plan_order() {
        let ops = vec![
            reinforce("A", 1, None, &["o1"]),
            reinforce("B", 2, None, &["o2"]),
            retract("C", 3, &["o3"]),
        ];
        let out = collapse(ops.clone());
        assert_eq!(out, ops, "nothing groups, nothing moves");
    }

    /// A candidate is a request for human review; merging two of them would
    /// decide something a person has not decided yet. It also writes no
    /// `memory_entry`, so it cannot conflict in the first place.
    #[test]
    fn propose_candidate_never_participates_even_when_it_names_the_same_entry() {
        let ops = vec![
            candidate("cand-1", "E"),
            reinforce("E", 3, None, &["o1"]),
            candidate("cand-2", "E"),
        ];
        let out = collapse(ops.clone());
        assert_eq!(out, ops, "both candidates and the reinforce survive");
    }

    /// `Create` mints its own `memory_id` inside `guard` and carries no
    /// `expected_version`; `Noop` writes nothing. Neither can collide, and
    /// keying them on their text would silently merge two distinct new
    /// entries.
    #[test]
    fn creates_and_noops_carry_no_target_and_are_never_collapsed() {
        let ops = vec![
            create("N1", "same text"),
            GeneratedOp::Noop,
            create("N2", "same text"),
            GeneratedOp::Noop,
        ];
        let out = collapse(ops.clone());
        assert_eq!(out, ops);
    }

    /// The structural invariant: whatever the rules decide, the output is the
    /// input with ops removed — never reordered, never rewritten in place.
    #[test]
    fn the_collapsed_plan_is_a_subsequence_of_the_input() {
        let ops = vec![
            reinforce("A", 1, None, &["o1"]),
            create("N1", "new"),
            reinforce("B", 2, None, &["o2"]),
            reinforce("A", 1, None, &["o3"]),
            GeneratedOp::Noop,
            retract("B", 2, &["o4"]),
            reinforce("C", 5, None, &["o5"]),
        ];
        let out = collapse(ops.clone());

        // Every surviving op appears in the input, in the same relative order,
        // with the same kind and target.
        let mut next = 0usize;
        for survivor in &out {
            let found = (next..ops.len())
                .find(|i| {
                    target_entry(&ops[*i]) == target_entry(survivor)
                        && std::mem::discriminant(&ops[*i]) == std::mem::discriminant(survivor)
                })
                .unwrap_or_else(|| panic!("survivor {survivor:?} is not in the input tail"));
            next = found + 1;
        }
        assert_eq!(
            out.len(),
            5,
            "A and B each collapse a pair; the create, the noops and C stay: {out:?}"
        );
    }
}
