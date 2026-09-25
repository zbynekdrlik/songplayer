//! Media-time A/V alignment of the PACED audio (#148 design v2).
//!
//! The pacer emits the video frame and its boundary audio block in the SAME
//! [`service`](super::Pacer::service) call, so the anchor is local: when a
//! FRESH frame is emitted after a (re-)align trigger, its media time and the
//! stamped boundary become the anchor, and the audio block at any later
//! boundary `B` must start at `anchor_media + (B − anchor_wall)` in samples.
//! That continues across 24/25→30 repeat boundaries, so correctly paired input
//! is never corrected.
//!
//! - **Hard (re-)alignment** (`anchor`, Resume, a lag re-anchor, a grid
//!   resync — every change of the wall↔media map): silence until a fresh frame
//!   is emitted, then drop early / pad late audio so the first non-silent block
//!   starts at the frame's media time ±1 sample
//!   ([`AudioGridBuffer::align_to`](crate::playback::audio_grid::AudioGridBuffer::align_to)).
//! - **Continuous correction** (every later productive boundary): past 5 ms of
//!   error drop/insert at most 48 samples per block until ≤ 1 ms
//!   ([`correction_for`]). This is the ONLY controller on the buffer — the old
//!   PLL level trim is gone.
//!
//! Audio enters the buffer when a frame is PULLED ([`Pacer::pull_frame`]),
//! not when it is consumed: the aligned take at boundary `B` needs media up to
//! `B + 33 ms`, which only the NEXT (parked) frame's paired audio covers.
//!
//! Split into a sibling of `pacer.rs` so that file stays under the 1000-line
//! cap; as a child module it reaches the `Pacer`'s private state directly.

use super::{AUDIO_GRID_RATE_HZ, PacedFrame, Pacer};
use crate::playback::audio_grid::{correction_for, samples_from_100ns};
use sp_ndi::AudioFrame;

/// Alignment state + telemetry for the paced audio (#148 design v2). The
/// default is "re-align pending", so a fresh pacer aligns on its first frame.
#[derive(Debug, Default)]
pub(super) struct AvAlign {
    /// `(media sample, wall 100 ns)` of the frame the audio is anchored to;
    /// `None` = waiting for a fresh frame after a (re-)align trigger.
    anchor: Option<(i64, i64)>,
    /// The hard alignment to the current anchor has completed.
    aligned: bool,
    /// The continuous correction is engaged (hysteresis state).
    engaged: bool,
    /// Head − expected media (samples) at the last measured boundary, before
    /// that boundary's correction. POSITIVE = audio ahead of the picture.
    last_err: i64,
    /// Boundaries on which samples were dropped or padded (cumulative).
    pub(super) corrections: u64,
    /// Total samples dropped + padded (cumulative).
    pub(super) corrected_samples: u64,
}

impl AvAlign {
    /// Forget the anchor: the next fresh frame re-aligns the audio (start,
    /// seek, Resume, lag re-anchor, grid resync).
    pub(super) fn realign(&mut self) {
        self.anchor = None;
        self.aligned = false;
        self.engaged = false;
    }

    /// Count one boundary's drop/pad of `delta` samples (0 = none).
    fn record(&mut self, delta: i64) {
        if delta != 0 {
            self.corrections += 1;
            self.corrected_samples += delta.unsigned_abs();
        }
    }

    /// The last measured A/V error in milliseconds (+ = audio ahead).
    pub(super) fn err_ms(&self) -> f64 {
        self.last_err as f64 * 1000.0 / AUDIO_GRID_RATE_HZ as f64
    }
}

/// Re-interleave a planar block into one [`AudioFrame`] (no timecode: the
/// submitter stamps the raw emit-instant wall clock, §6). An empty `planar`
/// (no channels seen yet) yields no frame, so the sink submits no audio.
pub(super) fn interleave(planar: Vec<Vec<f32>>) -> Vec<AudioFrame> {
    if planar.is_empty() {
        return Vec::new();
    }
    let channels = planar.len();
    let n = planar[0].len();
    let mut data = vec![0.0f32; channels * n];
    for (c, plane) in planar.iter().enumerate() {
        for (j, &s) in plane.iter().enumerate() {
            data[j * channels + c] = s;
        }
    }
    vec![AudioFrame {
        data,
        channels: channels as u32,
        sample_rate: AUDIO_GRID_RATE_HZ,
        timecode_100ns: None,
    }]
}

impl Pacer {
    /// Pull the next decoded frame and push its audio (with the chunk media
    /// times) into the grid buffer immediately. The returned frame's `audio` is
    /// emptied — it now lives in the buffer, and the sink reads only pixels.
    pub(super) fn pull_frame<F>(&mut self, pull: &mut F) -> Option<PacedFrame>
    where
        F: FnMut() -> Option<PacedFrame>,
    {
        let mut frame = pull()?;
        let audio = std::mem::take(&mut frame.audio);
        self.push_audio(&audio);
        Some(frame)
    }

    /// The audio block for a productive boundary stamped `stamp_100ns`.
    /// `fresh_pts_100ns` is the media time of the frame emitted on this
    /// boundary (`None` on a repeat). Untimed audio (no media head) plays as a
    /// plain FIFO; timed audio is hard-aligned after a trigger, then corrected
    /// continuously.
    pub(super) fn take_aligned_audio(
        &mut self,
        stamp_100ns: i64,
        fresh_pts_100ns: Option<i64>,
    ) -> Vec<AudioFrame> {
        let n = self.samples_per_boundary;
        let Some(head) = self.audio_buf.head_media() else {
            return interleave(self.audio_buf.take_boundary_chunk(n));
        };
        if self.av.anchor.is_none() {
            self.av.anchor = fresh_pts_100ns
                .map(|pts| (samples_from_100ns(pts, AUDIO_GRID_RATE_HZ), stamp_100ns));
        }
        let Some((media, wall)) = self.av.anchor else {
            // Re-align pending on a repeat boundary: silence until a fresh frame.
            return interleave(vec![vec![0.0; n]; self.audio_buf.channels()]);
        };
        let expected = media + samples_from_100ns(stamp_100ns - wall, AUDIO_GRID_RATE_HZ);
        self.av.last_err = head - expected;
        if self.av.aligned {
            let (extra, engaged) = correction_for(self.av.last_err, self.av.engaged);
            self.av.engaged = engaged;
            let (planar, applied) = self.audio_buf.take_block(n, extra);
            self.av.record(applied);
            return interleave(planar);
        }
        let (aligned, delta) = self.audio_buf.align_to(expected);
        self.av.aligned = aligned;
        self.av.record(delta);
        if aligned {
            interleave(self.audio_buf.take_boundary_chunk(n))
        } else {
            // Not enough audio buffered to drop yet: silence, retry next boundary.
            interleave(vec![vec![0.0; n]; self.audio_buf.channels()])
        }
    }
}
