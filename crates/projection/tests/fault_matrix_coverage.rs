//! T07-05: mechanically cross-checks the declared fault matrix
//! (`fixtures/fault/matrix.json`, spec 05 §10) against the Rust tests that
//! execute it — the "reusable artifact" the group card asks for. This file
//! runs without the `failpoints` feature (it only reads the fixture and a
//! static registry; it never itself exercises a failpoint), so it catches
//! drift — a renamed test, an edited fixture — on every plain `cargo test`,
//! not only the failpoints re-run.
//!
//! Dependency-free JSON scan (no `serde_json`, which is not an approved
//! dependency for this crate): mirrors
//! `crates/index/tests/language_coverage.rs`'s small-string-helper style.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    // crates/projection -> crates -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn read_matrix_json() -> String {
    let path = workspace_root().join("fixtures/fault/matrix.json");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every `"id": "F<N>"` row id declared under the F-matrix. The S (spool kill
/// matrix) rows use a distinct `id` prefix and are skipped; the F-matrix's own
/// top-level `"id": "F"` (no digits) is skipped too — only digited row ids
/// count.
fn declared_f_ids(json: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let needle = "\"id\": \"F";
    let mut rest = json;
    while let Some(pos) = rest.find(needle) {
        let after = &rest[pos + needle.len()..];
        let end = after.find('"').expect("closing quote after an id value");
        let digits = &after[..end];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            ids.insert(format!("F{digits}"));
        }
        rest = &after[end..];
    }
    ids
}

/// Every F-row this crate's test suite proves, mapped to exactly where (spec
/// 05 §10's "named test per row"). Cross-checked below against the declared
/// matrix — this table *is* the reusable, machine-checked coverage artifact:
/// drift here (a rename, a fixture edit) fails loudly instead of silently.
const FAULT_MATRIX_COVERAGE: &[(&str, &str)] = &[
    (
        "F1",
        "switch_faults.rs::backend_error_leaves_detectable_updating",
    ),
    (
        "F2",
        "fault_matrix.rs::f2_kill_mid_upsert_leaves_status_updating_and_stale_head",
    ),
    (
        "F3",
        "fault_matrix.rs::f3_kill_before_write_head_leaves_stale_op_id",
    ),
    (
        "F4",
        "fault_matrix.rs::f4_kill_before_final_commit_leaves_head_ahead_of_active",
    ),
    (
        "F5",
        "fault_matrix.rs::f5_post_clean_point_loss_detected_at_next_open",
    ),
    (
        "F6",
        "fault_matrix.rs::f6_partial_point_deletion_detected_at_next_open",
    ),
    (
        "F7",
        "fault_matrix.rs::f7_missing_head_detected_at_next_open",
    ),
    (
        "F8",
        "fault_matrix.rs::f8_equal_count_different_ids_detected_at_next_open",
    ),
    (
        "F9",
        "fault_matrix.rs::f9_backend_reported_success_but_corrupted_content_caught_at_next_open",
    ),
    (
        "F10",
        "fault_matrix.rs::f10_same_as_f5_backend_flush_failure_swallowed",
    ),
    (
        "F11",
        "rebuild_faults.rs::crash_during_rebuild_leaves_rebuilding_and_retry_converges",
    ),
    (
        "F12",
        "rebuild.rs::unopenable_shard_is_quarantined_and_rebuilt",
    ),
];

#[test]
fn every_declared_fault_row_has_exactly_one_executable_test() {
    let declared = declared_f_ids(&read_matrix_json());
    let covered: BTreeSet<String> = FAULT_MATRIX_COVERAGE
        .iter()
        .map(|(id, _)| id.to_string())
        .collect();

    assert_eq!(
        covered.len(),
        FAULT_MATRIX_COVERAGE.len(),
        "duplicate id in the coverage registry"
    );
    assert_eq!(declared.len(), 12, "spec 05 §10 declares exactly F1..F12");
    assert_eq!(
        declared, covered,
        "fixtures/fault/matrix.json's F-matrix and FAULT_MATRIX_COVERAGE must name exactly the \
         same rows — update whichever one drifted"
    );
}

#[test]
fn json_scan_helper_is_correct() {
    let sample = r#"{
      "matrices": [
        { "id": "F", "rows": [
          { "id": "F1", "injected_fault": "x", "expected_signal": "y" },
          { "id": "F12", "injected_fault": "x", "expected_signal": "y" }
        ]},
        { "id": "S", "rows": [
          { "id": "S1", "injected_fault": "x", "expected_signal": "y" }
        ]}
      ]
    }"#;
    let ids = declared_f_ids(sample);
    assert_eq!(
        ids,
        ["F1", "F12"].into_iter().map(String::from).collect(),
        "the bare matrix id \"F\" and every \"S\" row must be excluded"
    );
}
