//! #289 step 0: the emit-lateness histogram's buckets and counts.

use super::{EDGES_US, EmitLateHist, EmitLateness, bucket};

/// The edges the design asks for: sub-millisecond resolution where the
/// steady hand-off lives (p50 0.3 ms), then the measured peaks (8.5–9.8 ms,
/// 14.7–20.7 ms after a start) up to one slot.
#[test]
fn the_edges_resolve_below_a_millisecond_and_end_at_one_slot() {
    assert_eq!(
        EDGES_US,
        [
            0, 100, 250, 500, 1_000, 2_000, 3_000, 5_000, 7_500, 10_000, 15_000, 20_000, 25_000,
            33_333
        ]
    );
}

/// Every edge opens its own bucket; one µs under it is the bucket before.
#[test]
fn an_edge_belongs_to_the_bucket_it_opens() {
    for (i, &edge) in EDGES_US.iter().enumerate() {
        assert_eq!(bucket(edge), i, "edge {edge}");
        if i > 0 {
            assert_eq!(bucket(edge - 1), i - 1, "one under edge {edge}");
        }
    }
    assert_eq!(
        bucket(u64::MAX),
        EDGES_US.len() - 1,
        "a stall of any length"
    );
}

#[test]
fn each_emit_is_counted_in_its_bucket() {
    let mut h = EmitLateness::default();
    for late in [0, 99, 110, 110, 9_999, 33_333, 330_600] {
        h.push(late);
    }
    let mut counts = vec![0u64; EDGES_US.len()];
    counts[0] = 2;
    counts[1] = 2;
    counts[8] = 1;
    counts[13] = 2;
    assert_eq!(
        h.report(),
        EmitLateHist {
            edges_us: EDGES_US.to_vec(),
            counts,
        }
    );
}

#[test]
fn the_report_round_trips_through_its_json() {
    let mut h = EmitLateness::default();
    h.push(2_500);
    let json = serde_json::to_value(h.report()).unwrap();
    assert_eq!(json["edges_us"][13].as_u64(), Some(33_333));
    assert_eq!(json["counts"][5].as_u64(), Some(1));
    let back: EmitLateHist = serde_json::from_value(json).unwrap();
    assert_eq!(back, h.report());
}
