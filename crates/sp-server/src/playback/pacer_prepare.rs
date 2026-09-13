//! Decode-ahead for the boundary-paced scheduler (#147 lane 4).
//!
//! Box test 2 (2026-09-13) proved the pacer's cadence and re-anchor correct but
//! the emit ~10-27 ms LATE: `pipeline_paced` slept to the boundary and only THEN
//! decoded the due frame, so it left the box after its stamp (`ts_head_skew`
//! ≈ 74), and audio was pushed at that same late instant (underruns). Lane 4
//! moves the decode ahead: the pipeline calls [`Pacer::prepare`] right after each
//! emit — decode forward to the frame due at the NEXT boundary, drop older, park
//! the first beyond, and push every consumed frame's audio into the wall-clock
//! buffer — THEN sleeps, THEN [`service`](super::Pacer::service) submits the
//! already-decoded frame within ~1 ms of the boundary.
//!
//! Split into a sibling of `pacer.rs` so that file stays under the 1000-line cap;
//! as a child module it reaches the `Pacer`'s private state directly.

use super::{PacedFrame, Pacer};

impl Pacer {
    /// Decode FORWARD toward `target_boundary_100ns` (#147 lane 4): consume every
    /// buffered/pulled frame whose presentation time is at/before the boundary —
    /// pushing each consumed frame's audio into the wall-clock buffer — keep the
    /// LAST as the due frame ([`prepared`](super::Pacer)), count the earlier ones
    /// in `dropped` (e.g. 60→30 decimation), and park the first frame beyond the
    /// boundary in `pending`. Records the pre-decode duration into the prep ring
    /// (`prep_p99_us`) AND into `last_prep_100ns`, which `service` folds into the
    /// boundary's `iter` sample.
    ///
    /// Idempotent for a boundary already prepared (the parked `pending` is beyond
    /// it, so the loop breaks immediately and `prepared` stands). Genlock off
    /// (`interval == 0`) is a no-op. `pull` returns the next decoded frame or
    /// `None` at end-of-stream / decoder stall.
    pub fn prepare<F>(&mut self, target_boundary_100ns: i64, mut pull: F)
    where
        F: FnMut() -> Option<PacedFrame>,
    {
        if self.interval_100ns == 0 {
            return;
        }
        let start = self.now_100ns();
        loop {
            if self.pending.is_none() {
                self.pending = pull();
            }
            match self.pending.take() {
                Some(frame) => {
                    let present = self.wall_start_100ns.saturating_add(frame.pts_100ns());
                    if present <= target_boundary_100ns {
                        self.push_audio(&frame.audio);
                        if self.prepared.is_some() {
                            self.dropped += 1;
                        }
                        self.prepared = Some(frame);
                    } else {
                        self.pending = Some(frame);
                        break;
                    }
                }
                None => break,
            }
        }
        let elapsed = self.now_100ns().saturating_sub(start).max(0);
        self.last_prep_100ns = elapsed;
        self.push_prep((elapsed / 10) as u64);
    }

    /// Record one pre-decode (`prepare`) duration sample (µs) in the ring.
    fn push_prep(&mut self, cost_us: u64) {
        let cap = self.prep_ring.len();
        self.prep_ring[self.prep_idx] = cost_us;
        self.prep_idx = (self.prep_idx + 1) % cap;
        if self.prep_len < cap {
            self.prep_len += 1;
        }
    }

    /// 99th-percentile pre-decode duration (µs) over the recent ring. `>= interval`
    /// (≈ 33_333 µs @30 fps) means the decoder cannot produce a frame inside one
    /// slot and lag will grow until the re-anchor bounds it (#147 lane 4).
    pub fn prep_p99_us(&self) -> u64 {
        if self.prep_len == 0 {
            return 0;
        }
        let mut v: Vec<u64> = self.prep_ring[..self.prep_len].to_vec();
        v.sort_unstable();
        let idx = ((self.prep_len * 99) / 100).min(self.prep_len - 1);
        v[idx]
    }
}
