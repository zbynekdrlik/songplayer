//! Mutation-killing unit tests for `worker_reference.rs::gate_fail_reason_str`.
//!
//! Wired into `worker_reference.rs` as a sibling `#[path]` test module.

use super::*;
use crate::lyrics::reference_gate::GateFailReason;

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
