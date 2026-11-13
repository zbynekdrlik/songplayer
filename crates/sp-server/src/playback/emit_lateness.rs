//! #289 step 0 (design comment 6814927800): the pacer's emit lateness as a
//! histogram of fixed µs buckets, cumulative over the pipeline's life.
//!
//! The pacer already reports `max_late_us` and `jitter_p99_us`, but over a
//! 256-sample ring (≈ 8.5 s at 30 fps): the sub-slot DISTRIBUTION over hours
//! that sizes the rebuild's lead (one slot, 33.3 ms) cannot be read from
//! them. Every emit (`Pacer::on_emit`, lateness = emit − its boundary) is
//! counted into one bucket; `GET /api/v1/ndi/health` serves the counts per
//! pipeline as `pacing.emit_late_hist`. A reader diffs two polls for a
//! window. Telemetry only: nothing reads it back.

use serde::{Deserialize, Serialize};

/// Each bucket's lower edge, µs: bucket i holds `[EDGES_US[i],
/// EDGES_US[i + 1])`, the last one everything from one slot (33 333 µs) up.
pub const EDGES_US: [u64; 14] = [
    0, 100, 250, 500, 1_000, 2_000, 3_000, 5_000, 7_500, 10_000, 15_000, 20_000, 25_000, 33_333,
];

/// The bucket of a lateness of `late_us`: the last edge at or under it.
pub fn bucket(late_us: u64) -> usize {
    EDGES_US.partition_point(|&edge| edge <= late_us) - 1
}

/// The counts the pacer keeps (no allocation per emit).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmitLateness {
    counts: [u64; EDGES_US.len()],
}

impl EmitLateness {
    /// Count one emit `late_us` after its boundary.
    pub fn push(&mut self, late_us: u64) {
        self.counts[bucket(late_us)] += 1;
    }

    /// The histogram as the API serves it.
    pub fn report(&self) -> EmitLateHist {
        EmitLateHist {
            edges_us: EDGES_US.to_vec(),
            counts: self.counts.to_vec(),
        }
    }
}

/// `pacing.emit_late_hist` on `GET /api/v1/ndi/health`: `counts[i]` emits
/// were `edges_us[i]` µs or more, and under `edges_us[i + 1]` µs, late
/// (the last bucket has no upper edge). Both empty before the first
/// heartbeat.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmitLateHist {
    pub edges_us: Vec<u64>,
    pub counts: Vec<u64>,
}

#[cfg(test)]
#[path = "emit_lateness_tests.rs"]
mod tests;
