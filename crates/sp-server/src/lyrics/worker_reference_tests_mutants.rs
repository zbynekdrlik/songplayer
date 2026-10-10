//! Mutation-killing unit tests for `worker_reference.rs::gate_fail_reason_str`
//! and `reference_gate_audit_json`.
//!
//! Wired into `worker_reference.rs` as a sibling `#[path]` test module.

use super::*;
use crate::lyrics::reference_gate::{GateFailReason, GateStats};

/// Kills both `180:5` body replacements (`-> ""` and `-> "xyzzy"`). Each
/// `GateFailReason` variant maps to its exact lowercase literal — none of
/// which is "" or "xyzzy" — so covering all three variants makes a whole-body
/// replacement fail on the first assertion.
#[test]
fn gate_fail_reason_str_maps_each_variant_exactly() {
    assert_eq!(gate_fail_reason_str(&GateFailReason::Coverage), "coverage");
    assert_eq!(gate_fail_reason_str(&GateFailReason::Offset), "offset");
    assert_eq!(
        gate_fail_reason_str(&GateFailReason::Agreement),
        "agreement"
    );
}

fn stats(sung_covered_frac: f64, max_uncovered_sung_ms: u64) -> GateStats {
    GateStats {
        lines_total: 10,
        lines_matched: 9,
        lines_timed: 8,
        matched_frac: 0.9,
        median_signed_ms: 12,
        within_400_frac: 0.8,
        sung_words: 200,
        sung_covered_frac,
        max_uncovered_sung_ms,
    }
}

/// #144: the audit carries the sung numbers and whether they pass, so a
/// `coverage` failure of the sung direction reads apart from one of the
/// matched lines.
#[test]
fn the_audit_carries_the_sung_coverage_and_its_verdict() {
    let fail = reference_gate_audit_json(
        "fail",
        Some("coverage"),
        Some(&stats(0.58, 48_200)),
        Some("cpu"),
        Some(3.5),
        296,
    );
    assert_eq!(fail["sung_words"], 200);
    assert_eq!(fail["sung_covered_frac"], 0.58);
    assert_eq!(fail["max_uncovered_sung_ms"], 48_200);
    assert_eq!(fail["sung_coverage_ok"], false);
    assert_eq!(fail["matched_frac"], 0.9);
    // #144 F3: only the stage's own pre-mtl FAIL sets it.
    assert_eq!(fail["before_mtl"], false);

    let pass = reference_gate_audit_json("pass", None, Some(&stats(0.9, 800)), None, None, 1);
    assert_eq!(pass["sung_coverage_ok"], true);

    let error = reference_gate_audit_json("error", Some("mtl_align: x"), None, None, None, 0);
    assert_eq!(error["sung_coverage_ok"], serde_json::Value::Null);
    assert_eq!(error["sung_words"], 0);
}
