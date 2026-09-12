//! Boundary-paced emission scheduler (#147).
//!
//! The pure decision + counter engine that drives one video frame onto every
//! wall-clock grid boundary (camera-box#1294 §3/§5), replacing the NDI SDK's
//! free-running `clock_video` cadence. All scheduling decisions come from the
//! pure [`sp_core::genlock`] functions so the same logic is exercised on the
//! Linux CI job (via [`pacer_tests`]) as on the Windows decode loop.
//!
//! I/O is kept OUT of this module: the caller supplies the current wall clock
//! (`now_ns`), a way to pull the next decoded frame (`pull`), and a
//! [`PacedSink`] that performs the actual audio-before-video submission. In
//! production the sink is `FrameSubmitter` and `pull` is the MediaFoundation
//! decoder; in tests they are a recording fake + a synthetic frame stream, so
//! every emit/repeat/drop/catch-up/resync/re-latch decision is Linux-testable.

use sp_core::genlock::{
    boundary_skip_count, floor_boundary_100ns, genlock_emit_gate, interval_100ns, interval_ns,
};
use sp_ndi::AudioFrame;

use crate::playback::ndi_health::PacingStats;
use crate::playback::wallclock::WallClock;

/// A decoded frame handed to the pacer. `pts_ns` is the presentation time
/// measured from playback start (0-based); the pacer maps it onto the wall
/// grid via `wall_start_ns + pts_ns`.
#[derive(Clone, Debug)]
pub struct PacedFrame {
    pub pts_ns: i64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// NV12 pixel data.
    pub video: Vec<u8>,
    /// Audio chunk(s) belonging to this frame — submitted BEFORE the video.
    pub audio: Vec<AudioFrame>,
}

/// The sink the pacer emits through. `FrameSubmitter` implements it in
/// production (audio-before-video async NDI submit with explicit timecodes);
/// tests use a recording fake.
pub trait PacedSink {
    /// Emit one frame at a boundary: audio chunks first (stamped
    /// `audio_tc_100ns`, raw wall clock — §6), then the video frame (stamped
    /// `video_tc_100ns`, the floored boundary — §4).
    fn emit(&mut self, frame: &PacedFrame, video_tc_100ns: i64, audio_tc_100ns: i64);
}

/// What [`Pacer::service`] did on one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceOutcome {
    /// Not yet at the pending boundary — the caller sleeps until `until_ns`
    /// (wall-clock ns) then calls again.
    Wait { until_ns: i64 },
    /// A boundary was serviced by emitting a freshly-due decoded frame.
    Emitted,
    /// A boundary was serviced by repeating the last emitted frame (decoder
    /// underrun / sub-grid content) stamped at the NEW boundary (§5.5).
    Repeated,
    /// A boundary came due before the first frame was ever decoded — nothing to
    /// emit or repeat yet (pre-roll). The boundary still advances.
    Starved,
}

/// The jitter ring capacity (emit − boundary, µs).
const JITTER_RING: usize = 256;

/// Boundary-paced emission scheduler. Owns the wall clock, the grid geometry,
/// the pending/last decoded frames, and the telemetry counters.
pub struct Pacer {
    wall: WallClock,
    grid_fps: i64,
    interval_ns: i64,
    /// Next un-emitted boundary (wall-clock ns since the epoch); 0 = uninit.
    next_boundary_ns: i64,
    /// Playback anchor: the first boundary after play/seek. `present_ns(pts) =
    /// wall_start_ns + pts`.
    wall_start_ns: i64,
    /// A decoded frame parked because its presentation time is beyond the
    /// current boundary — serviced at a later boundary.
    pending: Option<PacedFrame>,
    /// Clone of the last emitted frame, re-sent on an underrun (§5.5).
    last_frame: Option<PacedFrame>,
    /// The last emitted video timecode (100-ns floored grid boundary). Used to
    /// keep stamps STRICTLY increasing across the ns-pacing vs 100-ns-stamp grid
    /// drift (a naive `floor` of two consecutive ns boundaries can collide onto
    /// one stamp slot). `None` until the first emit / after a re-anchor.
    last_video_tc: Option<i64>,
    enabled: bool,

    // --- telemetry counters (#149) ---
    seq: u64,
    late_frames: u64,
    max_late_us: u64,
    jitter_ring: [u64; JITTER_RING],
    jitter_len: usize,
    jitter_idx: usize,
    repeats: u64,
    resyncs: u64,
    relatches: u64,
    dropped: u64,
}

impl Pacer {
    /// Build a pacer for `grid_fps` (the fixed integer grid rate, e.g.
    /// `GENLOCK_GRID_FPS = 30`) over the production system wall clock.
    pub fn new(grid_fps: i64, enabled: bool) -> Self {
        Self::with_wallclock(grid_fps, enabled, WallClock::system())
    }

    /// Build a pacer over an injected [`WallClock`] (deterministic in tests).
    pub fn with_wallclock(grid_fps: i64, enabled: bool, wall: WallClock) -> Self {
        Self {
            wall,
            grid_fps,
            interval_ns: interval_ns(grid_fps),
            next_boundary_ns: 0,
            wall_start_ns: 0,
            pending: None,
            last_frame: None,
            last_video_tc: None,
            enabled,
            seq: 0,
            late_frames: 0,
            max_late_us: 0,
            jitter_ring: [0; JITTER_RING],
            jitter_len: 0,
            jitter_idx: 0,
            repeats: 0,
            resyncs: 0,
            relatches: 0,
            dropped: 0,
        }
    }

    /// Current wall clock as ns since the Unix epoch (production read path).
    pub fn now_ns(&self) -> i64 {
        self.wall.now_100ns().saturating_mul(100)
    }

    /// Advance the wall-clock resample counter (call once per emitted boundary).
    pub fn tick_wall(&mut self) {
        self.wall.tick();
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Anchor playback at `now_ns`: the first grid boundary strictly after
    /// `now_ns` becomes both `wall_start` (the PTS→grid origin) and the next
    /// un-emitted boundary. Clears any parked frame (a fresh seek). Called on
    /// play/seek.
    pub fn anchor(&mut self, now_ns: i64) {
        let first = if self.interval_ns == 0 {
            now_ns
        } else {
            now_ns - now_ns.rem_euclid(self.interval_ns) + self.interval_ns
        };
        self.wall_start_ns = first;
        self.next_boundary_ns = first;
        self.pending = None;
        self.last_video_tc = None;
    }

    /// The strictly-increasing 100-ns video stamp for a frame presented at wall
    /// time `now_ns`: the floored grid boundary (§4, FLOOR never ceil), bumped
    /// to the next grid boundary if the floor collides with the previous stamp
    /// (the ns-pacing vs 100-ns-stamp grid drift). Records the result.
    fn next_video_tc(&mut self, now_ns: i64) -> i64 {
        let floor_now = floor_boundary_100ns(now_ns / 100, self.grid_fps);
        let tc = match self.last_video_tc {
            // Collision / regression (the ns-pacing vs 100-ns-stamp grid drift):
            // advance to the FLOOR grid boundary strictly after `last`. Adding
            // one whole stamp interval + 1 lands inside the next slot; flooring
            // snaps it back onto the grid, so the stamp stays a valid FLOOR
            // boundary and is strictly greater than `last`.
            Some(last) if floor_now <= last => {
                floor_boundary_100ns(last + interval_100ns(self.grid_fps) + 1, self.grid_fps)
            }
            _ => floor_now,
        };
        self.last_video_tc = Some(tc);
        tc
    }

    /// Service one scheduling step at wall time `now_ns`.
    ///
    /// Returns [`ServiceOutcome::Wait`] when the pending boundary is still in
    /// the future (the caller sleeps to it, then spins the last ~2 ms). At/past
    /// a boundary it decodes forward per the presentation rule (drop
    /// older-than-boundary frames, park the first future frame), emits the
    /// freshly-due frame (or repeats the last on underrun), and advances the
    /// boundary via [`genlock_emit_gate`].
    pub fn service<F, S>(&mut self, now_ns: i64, mut pull: F, sink: &mut S) -> ServiceOutcome
    where
        F: FnMut() -> Option<PacedFrame>,
        S: PacedSink,
    {
        if self.interval_ns == 0 {
            // Genlock off — nothing to pace.
            return ServiceOutcome::Wait { until_ns: now_ns };
        }

        // A backward clock step re-latches the boundary (the pending boundary
        // sat more than one interval in the future). Count it once, here,
        // before the Wait check consumes the observation.
        if self.next_boundary_ns != 0 && self.next_boundary_ns > now_ns + self.interval_ns {
            self.relatches += 1;
        }

        let boundary = latched_boundary(now_ns, self.next_boundary_ns, self.interval_ns);
        if now_ns < boundary {
            self.next_boundary_ns = boundary;
            return ServiceOutcome::Wait { until_ns: boundary };
        }

        // Decode-forward: keep the last frame whose presentation time is at/before
        // this boundary; drop the older ones; park the first future frame.
        let mut due: Option<PacedFrame> = None;
        loop {
            if self.pending.is_none() {
                self.pending = pull();
            }
            match self.pending.take() {
                Some(frame) => {
                    let present = self.wall_start_ns.saturating_add(frame.pts_ns);
                    if present <= boundary {
                        if due.is_some() {
                            self.dropped += 1;
                        }
                        due = Some(frame);
                    } else {
                        self.pending = Some(frame);
                        break;
                    }
                }
                None => break,
            }
        }

        // Audio carries the raw wall clock (§6); video the floored, strictly
        // increasing grid stamp (§4), computed only when a frame is emitted.
        let audio_tc = now_ns / 100;

        let had_frame = due.is_some();
        let outcome = if let Some(frame) = due {
            self.on_emit(now_ns, boundary);
            let video_tc = self.next_video_tc(now_ns);
            sink.emit(&frame, video_tc, audio_tc);
            self.last_frame = Some(frame);
            ServiceOutcome::Emitted
        } else if self.last_frame.is_some() {
            self.on_emit(now_ns, boundary);
            self.repeats += 1;
            let video_tc = self.next_video_tc(now_ns);
            // Borrow-checker: pull the clone out, emit, put it back.
            let lf = self.last_frame.take().unwrap();
            sink.emit(&lf, video_tc, audio_tc);
            self.last_frame = Some(lf);
            ServiceOutcome::Repeated
        } else {
            ServiceOutcome::Starved
        };

        // Advance the boundary. `queue_had_frame` = a real frame is buffered
        // (just consumed, or parked) so a large lag catches up rather than
        // resyncing past buffered content (#1131).
        let queue_had_frame = had_frame || self.pending.is_some();
        let (_, next) = genlock_emit_gate(
            now_ns,
            self.next_boundary_ns,
            self.interval_ns,
            queue_had_frame,
        );
        if boundary_skip_count(self.next_boundary_ns, next, self.interval_ns) > 0 {
            self.resyncs += 1;
        }
        self.next_boundary_ns = next;

        outcome
    }

    fn on_emit(&mut self, now_ns: i64, boundary_ns: i64) {
        self.seq += 1;
        let late_ns = (now_ns - boundary_ns).max(0);
        let late_us = (late_ns / 1_000) as u64;
        self.push_jitter(late_us);
        if late_ns >= self.interval_ns {
            self.late_frames += 1;
        }
        if late_us > self.max_late_us {
            self.max_late_us = late_us;
        }
    }

    fn push_jitter(&mut self, late_us: u64) {
        self.jitter_ring[self.jitter_idx] = late_us;
        self.jitter_idx = (self.jitter_idx + 1) % JITTER_RING;
        if self.jitter_len < JITTER_RING {
            self.jitter_len += 1;
        }
    }

    /// 99th-percentile jitter (µs) over the recent ring.
    fn jitter_p99_us(&self) -> u64 {
        if self.jitter_len == 0 {
            return 0;
        }
        let mut v: Vec<u64> = self.jitter_ring[..self.jitter_len].to_vec();
        v.sort_unstable();
        // Index of the 99th percentile: 99% of samples are at or below it.
        let idx = ((self.jitter_len * 99) / 100).min(self.jitter_len - 1);
        v[idx]
    }

    /// Snapshot the counters for the health document.
    pub fn stats(&self) -> PacingStats {
        PacingStats {
            enabled: self.enabled,
            seq: self.seq,
            late_frames: self.late_frames,
            max_late_us: self.max_late_us,
            jitter_p99_us: self.jitter_p99_us(),
            repeats: self.repeats,
            resyncs: self.resyncs,
            relatches: self.relatches,
            dropped: self.dropped,
        }
    }
}

/// The boundary [`genlock_emit_gate`] latches for `now_ns` — re-exported shape
/// of `sp_core::genlock`'s private helper so the pacer's Wait/service split uses
/// the IDENTICAL boundary the gate advances from. Init (`next == 0`) or a
/// backward step (`next > now + interval`) re-latches to the next boundary
/// above `now`; otherwise the pending boundary stands.
fn latched_boundary(now_ns: i64, next_boundary_ns: i64, interval_ns: i64) -> i64 {
    if next_boundary_ns == 0 || next_boundary_ns > now_ns + interval_ns {
        now_ns - now_ns.rem_euclid(interval_ns) + interval_ns
    } else {
        next_boundary_ns
    }
}

#[cfg(test)]
#[path = "pacer_tests.rs"]
mod pacer_tests;
