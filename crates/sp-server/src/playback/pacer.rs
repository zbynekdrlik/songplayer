//! Boundary-paced emission scheduler (#147).
//!
//! The pure decision + counter engine that drives one video frame onto every
//! wall-clock grid boundary (camera-box#1294 §3/§5), replacing the NDI SDK's
//! free-running `clock_video` cadence.
//!
//! **Exact 100-ns grid (rework, dev.11).** SongPlayer GENERATES its own timing:
//! it stamps the video timecode on the second-anchored exact-rational 100-ns
//! grid (`floor_boundary_100ns`) AND it must sleep to those same boundaries, so
//! pacing and stamps share ONE grid. The pacer therefore works entirely in
//! 100-ns units and drives the exact-grid [`genlock_emit_gate_100ns`] twin (not
//! the uniform-ns reference gate camera-box uses to decimate arrivals). The
//! serviced boundary IS the stamp — already on-grid, strictly increasing, and
//! never future-dated — so there is no `floor(now)` at emission and no
//! monotonicity guard.
//!
//! I/O is kept thin: the pacer OWNS its [`WallClock`] and reads it when it needs
//! it (a scheduling read at entry, an emit read right before the send so
//! lateness includes decode time). The caller supplies a way to pull the next
//! decoded frame (`pull`) and a [`PacedSink`] that performs the actual
//! audio-before-video submission. In production the sink is `FrameSubmitter` and
//! `pull` is the MediaFoundation decoder; in tests they are a recording fake + a
//! synthetic frame stream over a settable clock, so every
//! emit/repeat/drop/catch-up/resync/re-latch decision is Linux-testable.

use sp_core::genlock::{
    GENLOCK_MAX_CATCHUP_INTERVALS, UNITS_PER_SECOND, floor_boundary_100ns, genlock_emit_gate_100ns,
    interval_100ns, lag_slots_100ns, strict_next_boundary_100ns,
};
use sp_ndi::AudioFrame;

/// A playing lag beyond [`GENLOCK_MAX_CATCHUP_INTERVALS`] must persist this long
/// (100-ns units, 1 s) before the pacer re-anchors (#147 lane 3, change 2). A
/// short catch-up burst (a slow keyframe, a scheduling hiccup) is absorbed by
/// the one-slot-per-iteration catch-up; only a decoder that stays behind
/// (`iter_cost >= interval`) trips the re-anchor.
const LAG_REANCHOR_AFTER_100NS: i64 = 10_000_000;

/// An emit lands "late" for [`PacingStats::late_frames`] only when it is more
/// than this (100-ns units, 2 ms) past its boundary (#147 lane 3, change 3). The
/// old gate counted a full interval late — so a decoder that drifts 3 ms/frame
/// registered ZERO late frames while lag grew unbounded. 2 ms is the receiver's
/// latency floor, so anything later genuinely risks the present deadline.
const LATE_THRESHOLD_100NS: i64 = 20_000;

use crate::playback::ndi_health::PacingStats;
use crate::playback::wallclock::WallClock;

/// A decoded frame handed to the pacer. `pts_ns` is the presentation time
/// measured from playback start (0-based); the pacer maps it onto the wall grid
/// via `wall_start_100ns + pts_ns / 100`.
#[derive(Clone, Debug)]
pub struct PacedFrame {
    pub pts_ns: i64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// NV12 pixel data.
    pub video: Vec<u8>,
    /// Audio chunk(s) belonging to this frame. Drained into the boundary's audio
    /// batch when the frame is CONSUMED (emitted OR dropped/decimated) and
    /// submitted before that boundary's video frame (§6).
    pub audio: Vec<AudioFrame>,
}

impl PacedFrame {
    /// Presentation time in 100-ns units (0-based). `pts_ns` comes from
    /// milliseconds (`ms * 1_000_000`), so the `/100` is exact.
    fn pts_100ns(&self) -> i64 {
        self.pts_ns / 100
    }
}

/// The sink the pacer emits through. `FrameSubmitter` implements it in
/// production (audio-before-video async NDI submit with explicit timecodes);
/// tests use a recording fake.
pub trait PacedSink {
    /// Emit one boundary: submit each chunk in `audio` (stamped `audio_tc_100ns`,
    /// the raw wall clock at submission — §6) IN ORDER first, then the `video`
    /// frame (stamped `video_tc_100ns`, the on-grid serviced boundary — §4). A
    /// starvation repeat passes an empty `audio` slice (§6.4 — a repeat submits
    /// no audio). Only `video`'s pixel fields are used; its own `audio` is
    /// ignored (the batch in `audio` already carries every consumed frame's
    /// chunks).
    fn emit(
        &mut self,
        video: &PacedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    );
}

/// What [`Pacer::service`] did on one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceOutcome {
    /// Not yet at the pending boundary — the caller sleeps until `until_100ns`
    /// (wall-clock 100 ns) then calls again.
    Wait { until_100ns: i64 },
    /// A boundary was serviced by emitting a freshly-due decoded frame.
    Emitted,
    /// A boundary was serviced by repeating the last emitted frame (decoder
    /// underrun / sub-grid content) stamped at the serviced boundary (§5.5).
    Repeated,
    /// A boundary came due before the first frame was ever decoded — nothing to
    /// emit or repeat yet (pre-roll). The boundary still advances.
    Starved,
    /// Playback fell behind by more than [`GENLOCK_MAX_CATCHUP_INTERVALS`] slots
    /// for over [`LAG_REANCHOR_AFTER_100NS`] continuously with a frame buffered
    /// (a decoder that cannot keep up): the grid was RE-ANCHORED so the buffered
    /// frame is due at `until_100ns` (the next real boundary), keeping stamps
    /// near `now` instead of drifting arbitrarily far behind. No frame is emitted
    /// or skipped on this call — content resumes from the buffered frame at
    /// `until_100ns`. The caller sleeps to `until_100ns` and WARNs with
    /// `lag_slots` (it holds the `playlist_id`). Counted as a `resync` (#147
    /// lane 3, change 2).
    Reanchored { lag_slots: i64, until_100ns: i64 },
}

/// What a STANDBY (paused / idle) boundary presents, for
/// [`Pacer::service_standby`]. The paced pipeline fills EVERY grid boundary
/// while paused/idle so the receiver stays `locked=` instead of seeing holes
/// (#147 fix-lane-2).
pub enum Standby<'a> {
    /// Paused: repeat the last real emitted frame (the frozen picture). Counts
    /// as a frozen-frame `repeat`; STARVES if nothing was ever emitted.
    FrozenLast,
    /// Idle / no song: present the supplied black frame. NOT a repeat (there is
    /// no real last frame to hold).
    Black(&'a PacedFrame),
}

/// The pure sleep-plan decision (#147 change 4), factored out so it is testable
/// without a live decode loop. `SystemClock` jumps and bad boundaries must never
/// park the send thread unboundedly, so the coarse sleep is clamped to
/// `[0, 1 s]` (camera-box `ndi.rs`), and a wall clock that sits MORE than one
/// interval before the boundary is a backward jump → `relatch` so the caller
/// re-latches instead of spin-waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SleepDecision {
    /// Coarse monotonic sleep in 100-ns units, clamped to `[0, 1 s]`.
    pub sleep_100ns: i64,
    /// The boundary sits more than one interval ahead of `now` — the clock
    /// stepped backward; do not spin, let the loop re-latch.
    pub relatch: bool,
}

/// Compute the [`SleepDecision`] for a wall clock at `now_100ns` targeting the
/// boundary `until_100ns` on a grid of `interval_100ns`. Pure; see
/// [`SleepDecision`].
pub fn plan_sleep_100ns(now_100ns: i64, until_100ns: i64, interval_100ns: i64) -> SleepDecision {
    let delta = until_100ns - now_100ns;
    SleepDecision {
        sleep_100ns: delta.clamp(0, UNITS_PER_SECOND),
        // The pacer only ever waits to the IMMEDIATE next boundary, so a normal
        // wait has `delta` within ONE grid slot. The exact-rational grid has ten
        // 333_334-wide slots per second (one tick wider than the nominal
        // `interval_100ns`, 333_333 @30 fps), so the bound is `interval + 2`
        // (slot width + a tick of margin) — NOT `> interval`, which mis-flagged
        // every wide slot as a backward jump and burned a self-clearing spin
        // (fix-lane-2 off-by-one). A larger gap is a genuine backward clock jump
        // (boundary far ahead). `interval == 0` (genlock off) never relatches —
        // it just sleeps the clamped 1 s.
        relatch: interval_100ns > 0 && delta > interval_100ns + 2,
    }
}

/// The jitter ring capacity (emit − boundary, µs).
const JITTER_RING: usize = 256;

/// The iteration-cost ring capacity (decode+submit per emit, µs) — #147 lane 3.
const ITER_RING: usize = 256;

/// The exact-grid boundary the emit gate latches for `now_100ns` — the same
/// decision [`genlock_emit_gate_100ns`] makes internally, computed here so the
/// pacer's Wait/emit split and its relatch counter use the IDENTICAL boundary
/// the gate advances from. Init (`nb == 0`) or a backward step
/// (`nb > floor(now) + interval`) re-latches to the next grid boundary strictly
/// after `now`; otherwise the pending boundary stands.
fn latched_boundary_100ns(now_100ns: i64, next_boundary_100ns: i64, fps: i64) -> i64 {
    let interval = interval_100ns(fps);
    if next_boundary_100ns == 0
        || next_boundary_100ns > floor_boundary_100ns(now_100ns, fps) + interval
    {
        strict_next_boundary_100ns(now_100ns, fps)
    } else {
        next_boundary_100ns
    }
}

/// Boundary-paced emission scheduler. Owns the wall clock, the grid geometry,
/// the pending/last decoded frames, and the telemetry counters.
pub struct Pacer {
    wall: WallClock,
    grid_fps: i64,
    interval_100ns: i64,
    /// Next un-emitted boundary (wall-clock 100 ns since the epoch); 0 = uninit.
    next_boundary_100ns: i64,
    /// Playback anchor: the first grid boundary strictly after play/seek.
    /// `present_100ns(pts) = wall_start_100ns + pts / 100`.
    wall_start_100ns: i64,
    /// A decoded frame parked because its presentation time is beyond the
    /// current boundary — serviced at a later boundary.
    pending: Option<PacedFrame>,
    /// The last emitted frame (video only; its audio was drained on emit),
    /// re-sent on an underrun (§5.5).
    last_frame: Option<PacedFrame>,
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
    /// Lag (whole grid slots the serviced boundary sat behind `floor(now)`) at
    /// the last emit — the health gauge and the re-anchor input (#147 lane 3).
    last_lag_slots: i64,
    /// Worst `last_lag_slots` this song, for the per-song summary.
    max_lag_slots: i64,
    /// Wall clock (100 ns) at which the lag first exceeded the catch-up bound and
    /// has stayed over it since; `None` while lag is within bound. Drives the
    /// 1 s sustain gate before a playback re-anchor (change 2).
    lag_exceeded_since: Option<i64>,
    // per-iteration decode+submit cost ring (µs), for iter_p50/p99.
    iter_ring: [u64; ITER_RING],
    iter_len: usize,
    iter_idx: usize,
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
            interval_100ns: interval_100ns(grid_fps),
            next_boundary_100ns: 0,
            wall_start_100ns: 0,
            pending: None,
            last_frame: None,
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
            last_lag_slots: 0,
            max_lag_slots: 0,
            lag_exceeded_since: None,
            iter_ring: [0; ITER_RING],
            iter_len: 0,
            iter_idx: 0,
        }
    }

    /// Current wall clock as 100-ns units since the Unix epoch (production read
    /// path — MediaFoundation decode + wall grid share this).
    pub fn now_100ns(&self) -> i64 {
        self.wall.now_100ns()
    }

    /// One grid interval in 100-ns units (`1e7 / grid_fps`); 0 = genlock off.
    pub fn interval_100ns(&self) -> i64 {
        self.interval_100ns
    }

    /// Advance the wall-clock resample counter (call once per serviced boundary).
    pub fn tick_wall(&mut self) {
        self.wall.tick();
    }

    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Anchor playback at the current wall clock: the first grid boundary
    /// strictly after `now` becomes both `wall_start` (the PTS→grid origin) and
    /// the next un-emitted boundary. Clears any parked frame AND the last
    /// emitted frame — a fresh seek / new song, so a starvation repeat can never
    /// show the PREVIOUS song's frame (#147 fix-lane-2, change 4). Called on
    /// play/seek.
    pub fn anchor(&mut self) {
        let now = self.now_100ns();
        let first = if self.interval_100ns == 0 {
            now
        } else {
            strict_next_boundary_100ns(now, self.grid_fps)
        };
        self.wall_start_100ns = first;
        self.next_boundary_100ns = first;
        self.pending = None;
        self.last_frame = None;
        // Fresh origin (play/seek/new song): the lag gauge + the re-anchor
        // sustain timer are per-song (#147 lane 3). Cumulative counters (seq,
        // repeats, …) intentionally survive — the health doc reads them.
        self.last_lag_slots = 0;
        self.max_lag_slots = 0;
        self.lag_exceeded_since = None;
    }

    /// Service one scheduling step. Reads the wall clock itself: a scheduling
    /// read decides Wait-vs-emit, and — when a boundary is due — a fresh emit
    /// read taken right before the send makes lateness include decode time
    /// (#147 change 5). The serviced boundary is the video stamp (on-grid,
    /// never future-dated); audio carries the raw emit-instant wall clock (§6).
    pub fn service<F, S>(&mut self, mut pull: F, sink: &mut S) -> ServiceOutcome
    where
        F: FnMut() -> Option<PacedFrame>,
        S: PacedSink,
    {
        if self.interval_100ns == 0 {
            // Genlock off — never spin: wait a full second (#147 change 8).
            let now = self.now_100ns();
            return ServiceOutcome::Wait {
                until_100ns: now.saturating_add(UNITS_PER_SECOND),
            };
        }

        let sched_now = self.now_100ns();
        let boundary = latched_boundary_100ns(sched_now, self.next_boundary_100ns, self.grid_fps);
        // A backward clock step re-latched the boundary to an EARLIER grid slot
        // (the pending boundary sat far in the future). Count it once.
        if self.next_boundary_100ns != 0 && boundary < self.next_boundary_100ns {
            self.relatches += 1;
        }
        if sched_now < boundary {
            self.next_boundary_100ns = boundary;
            return ServiceOutcome::Wait {
                until_100ns: boundary,
            };
        }

        // Bounded-lag re-anchor for PLAYBACK (#147 lane 3, change 2). Catch-up
        // advances the serviced boundary one slot per call, but each emitting
        // call also costs one decoder `pull`; when the file's per-frame cost is
        // >= interval the boundary can never gain on the wall clock and the stamp
        // drifts arbitrarily far behind `now`. camera-box's "buffered never
        // resyncs" is a capture-side rule (a live grabber can't outrun the wall
        // clock); a file decoder can, so a lag over the catch-up bound sustained
        // > 1 s WITH a frame buffered re-anchors the grid: the buffered frame
        // becomes due at the next real boundary, content resumes from it (no
        // skip, no burst), and the stamps snap back to `now`.
        let floor_now = floor_boundary_100ns(sched_now, self.grid_fps);
        let lag = lag_slots_100ns(boundary, floor_now, self.grid_fps);
        if lag > GENLOCK_MAX_CATCHUP_INTERVALS {
            let since = *self.lag_exceeded_since.get_or_insert(sched_now);
            if sched_now.saturating_sub(since) > LAG_REANCHOR_AFTER_100NS {
                if self.pending.is_none() {
                    self.pending = pull();
                }
                if let Some(pts) = self.pending.as_ref().map(|f| f.pts_100ns()) {
                    let new_boundary = strict_next_boundary_100ns(sched_now, self.grid_fps);
                    self.wall_start_100ns = new_boundary - pts;
                    self.next_boundary_100ns = new_boundary;
                    self.resyncs += 1;
                    self.lag_exceeded_since = None;
                    self.last_lag_slots = lag;
                    self.max_lag_slots = self.max_lag_slots.max(lag);
                    return ServiceOutcome::Reanchored {
                        lag_slots: lag,
                        until_100ns: new_boundary,
                    };
                }
                // No frame buffered → not a re-anchor case; the underrun/resync
                // path (below) handles it via `!queue_had_frame`.
            }
        } else {
            self.lag_exceeded_since = None;
        }

        // Decode-forward: keep the last frame whose presentation time is at/before
        // this boundary; drop the older ones (their audio still ships); park the
        // first future frame. Audio of EVERY consumed frame is batched (§6.4).
        let mut due: Option<PacedFrame> = None;
        let mut audio_batch: Vec<AudioFrame> = Vec::new();
        loop {
            if self.pending.is_none() {
                self.pending = pull();
            }
            match self.pending.take() {
                Some(mut frame) => {
                    let present = self.wall_start_100ns.saturating_add(frame.pts_100ns());
                    if present <= boundary {
                        audio_batch.append(&mut frame.audio);
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

        // Emit read: taken AFTER decode so lateness includes decode time (§5).
        let emit_now = self.now_100ns();

        // Never future-date the stamp (#147 change 2): if the clock stepped
        // backward during decode so the boundary is now in the future, re-latch
        // instead of emitting. (Impossible on the monotonic production clock
        // within one call; a defensive net for a clock reset / test.)
        if emit_now < boundary {
            if let Some(f) = due {
                self.pending = Some(f);
            }
            self.relatches += 1;
            let relatched =
                latched_boundary_100ns(emit_now, self.next_boundary_100ns, self.grid_fps);
            self.next_boundary_100ns = relatched;
            return ServiceOutcome::Wait {
                until_100ns: relatched,
            };
        }

        // Audio carries the raw emit-instant wall clock (§6). Resolve the stamp
        // boundary + the advance via the exact-grid gate BEFORE emitting, so a
        // resync stamps at the resync SERVICE boundary (the grid boundary
        // at/before now) rather than the stale pending boundary (§5.5).
        // `queue_had_frame` = a real frame is buffered (just consumed, or
        // parked) so a large lag catches up rather than resyncing past buffered
        // content (#1131).
        let audio_tc = emit_now;
        let had_frame = due.is_some();
        let queue_had_frame = had_frame || self.pending.is_some();
        let (stamp_boundary, next) =
            self.resolve_emit_boundary(emit_now, boundary, queue_had_frame);

        let outcome = if let Some(frame) = due {
            self.on_emit(emit_now, stamp_boundary);
            sink.emit(&frame, &audio_batch, stamp_boundary, audio_tc);
            self.last_frame = Some(frame);
            ServiceOutcome::Emitted
        } else if let Some(lf) = self.last_frame.take() {
            self.on_emit(emit_now, stamp_boundary);
            self.repeats += 1;
            // Starvation repeat: NO audio (§6.4).
            sink.emit(&lf, &[], stamp_boundary, audio_tc);
            self.last_frame = Some(lf);
            ServiceOutcome::Repeated
        } else {
            ServiceOutcome::Starved
        };

        // Telemetry for a productive boundary (#147 lane 3, change 3): the lag
        // gauge at THIS emit — how many whole slots the STAMPED boundary sat
        // behind `floor(now)`. On a slow-decode catch-up (the box-test-1 bug)
        // the stamp is the past serviced boundary, so the growing lag is visible;
        // on a resync the stamp is `floor(now)`, so lag reads ~0 (we jumped to
        // now — the `resyncs` counter already records it). Plus the decode+submit
        // iteration cost from the scheduling read to after the send: `iter_p99 >=
        // interval` is the honest "decoder can't keep up" signal.
        if matches!(outcome, ServiceOutcome::Emitted | ServiceOutcome::Repeated) {
            let emit_lag = lag_slots_100ns(
                stamp_boundary,
                floor_boundary_100ns(emit_now, self.grid_fps),
                self.grid_fps,
            );
            self.last_lag_slots = emit_lag;
            self.max_lag_slots = self.max_lag_slots.max(emit_lag);
            let done = self.now_100ns();
            self.push_iter((done.saturating_sub(sched_now).max(0) / 10) as u64);
        }

        self.next_boundary_100ns = next;
        outcome
    }

    /// Resolve the video stamp boundary and the next pending boundary for an
    /// emit at `emit_now` having latched `boundary`, given whether a real frame
    /// is buffered. Shared by [`service`](Self::service) and
    /// [`service_standby`](Self::service_standby).
    ///
    /// On a grid RESYNC — the gate leaped MORE than one slot past `boundary`
    /// (lag > `GENLOCK_MAX_CATCHUP_INTERVALS` with nothing buffered) — the stale
    /// pending `boundary` sits far in the past, so the stamp becomes the resync
    /// service boundary `floor_boundary_100ns(emit_now)` (on-grid and `<= now`),
    /// NEVER the stale boundary, and `resyncs` is bumped (§5.5, #147 fix-lane-2).
    /// A normal one-slot advance keeps the serviced `boundary` as the stamp.
    /// Returns `(stamp_boundary, next_boundary)`.
    fn resolve_emit_boundary(
        &mut self,
        emit_now: i64,
        boundary: i64,
        queue_had_frame: bool,
    ) -> (i64, i64) {
        let catch_up = strict_next_boundary_100ns(boundary, self.grid_fps);
        let (_, next) = genlock_emit_gate_100ns(emit_now, boundary, self.grid_fps, queue_had_frame);
        if next > catch_up {
            // Advanced more than one slot → a grid resync (skipped boundaries).
            self.resyncs += 1;
            (floor_boundary_100ns(emit_now, self.grid_fps), next)
        } else {
            (boundary, next)
        }
    }

    /// Service one STANDBY scheduling step: fill the current grid boundary with
    /// the frozen last frame (paused) or a black frame (idle / no song), stamped
    /// on-grid via the SAME machinery as [`service`](Self::service) but with NO
    /// audio and NO decode pull. The paced pipeline's paused branch and the
    /// paced idle loop call this once per boundary so EVERY boundary carries a
    /// frame while paused/idle — the receiver stays `locked=` instead of seeing
    /// holes (#147 fix-lane-2, change 2). Play/Seek re-anchor via
    /// [`anchor`](Self::anchor). Returns [`ServiceOutcome::Wait`] until the
    /// boundary is due, then [`ServiceOutcome::Repeated`] (frozen) /
    /// [`ServiceOutcome::Emitted`] (black), or [`ServiceOutcome::Starved`] when
    /// a frozen standby has no last frame yet.
    pub fn service_standby<S>(&mut self, standby: Standby, sink: &mut S) -> ServiceOutcome
    where
        S: PacedSink,
    {
        if self.interval_100ns == 0 {
            // Genlock off — never spin: wait a full second (#147 change 8).
            let now = self.now_100ns();
            return ServiceOutcome::Wait {
                until_100ns: now.saturating_add(UNITS_PER_SECOND),
            };
        }

        let sched_now = self.now_100ns();
        let boundary = latched_boundary_100ns(sched_now, self.next_boundary_100ns, self.grid_fps);
        if self.next_boundary_100ns != 0 && boundary < self.next_boundary_100ns {
            self.relatches += 1;
        }
        if sched_now < boundary {
            self.next_boundary_100ns = boundary;
            return ServiceOutcome::Wait {
                until_100ns: boundary,
            };
        }

        let emit_now = self.now_100ns();
        if emit_now < boundary {
            // Backward clock step during the scheduling read — re-latch, never
            // future-date (mirrors `service`).
            self.relatches += 1;
            let relatched =
                latched_boundary_100ns(emit_now, self.next_boundary_100ns, self.grid_fps);
            self.next_boundary_100ns = relatched;
            return ServiceOutcome::Wait {
                until_100ns: relatched,
            };
        }

        // Standby has no decode queue (`queue_had_frame = false`), so a long
        // stall resyncs and the stamp lands on the resync service boundary,
        // exactly like the active underrun path.
        let (stamp_boundary, next) = self.resolve_emit_boundary(emit_now, boundary, false);
        let audio_tc = emit_now;

        let outcome = match standby {
            Standby::FrozenLast => {
                if let Some(lf) = self.last_frame.take() {
                    self.on_emit(emit_now, stamp_boundary);
                    self.repeats += 1;
                    sink.emit(&lf, &[], stamp_boundary, audio_tc);
                    self.last_frame = Some(lf);
                    ServiceOutcome::Repeated
                } else {
                    ServiceOutcome::Starved
                }
            }
            Standby::Black(frame) => {
                self.on_emit(emit_now, stamp_boundary);
                sink.emit(frame, &[], stamp_boundary, audio_tc);
                ServiceOutcome::Emitted
            }
        };

        self.next_boundary_100ns = next;
        outcome
    }

    fn on_emit(&mut self, now_100ns: i64, boundary_100ns: i64) {
        self.seq += 1;
        let late_100ns = (now_100ns - boundary_100ns).max(0);
        let late_us = (late_100ns / 10) as u64; // 100 ns → µs
        self.push_jitter(late_us);
        // #147 lane 3, change 3: an emit is "late" past the 2 ms threshold, not
        // only a full interval late — a decoder drifting a few ms/frame used to
        // register ZERO late frames while its lag grew unbounded.
        if late_100ns > LATE_THRESHOLD_100NS {
            self.late_frames += 1;
        }
        if late_us > self.max_late_us {
            self.max_late_us = late_us;
        }
    }

    /// Record one per-iteration decode+submit cost sample (µs) in the ring.
    fn push_iter(&mut self, cost_us: u64) {
        self.iter_ring[self.iter_idx] = cost_us;
        self.iter_idx = (self.iter_idx + 1) % ITER_RING;
        if self.iter_len < ITER_RING {
            self.iter_len += 1;
        }
    }

    /// The `p`-th percentile (0..=100) of the iteration-cost ring (µs).
    fn iter_percentile_us(&self, p: usize) -> u64 {
        if self.iter_len == 0 {
            return 0;
        }
        let mut v: Vec<u64> = self.iter_ring[..self.iter_len].to_vec();
        v.sort_unstable();
        let idx = ((self.iter_len * p) / 100).min(self.iter_len - 1);
        v[idx]
    }

    /// 50th-percentile iteration cost (µs) — the per-song-summary median.
    pub fn iter_p50_us(&self) -> u64 {
        self.iter_percentile_us(50)
    }

    /// 99th-percentile iteration cost (µs). `>= interval` (≈ 33_333 µs @30 fps)
    /// means the decoder cannot keep up and lag will grow until the re-anchor.
    pub fn iter_p99_us(&self) -> u64 {
        self.iter_percentile_us(99)
    }

    /// Worst per-emit lag (whole grid slots) this song, for the summary.
    pub fn max_lag_slots(&self) -> i64 {
        self.max_lag_slots
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
            lag_slots: self.last_lag_slots,
            iter_p99_us: self.iter_p99_us(),
        }
    }
}

#[cfg(test)]
#[path = "pacer_tests.rs"]
mod pacer_tests;

#[cfg(test)]
#[path = "pacer_tests_lane3.rs"]
mod pacer_tests_lane3;

#[cfg(test)]
#[path = "pacer_tests_audio.rs"]
mod pacer_tests_audio;
