//! Media-time A/V alignment of the PACED audio (#148 design v2).
//!
//! The pacer emits the video frame and its boundary audio block in the SAME
//! [`service`](super::Pacer::service) call, so the anchor is local: the first
//! FRESH frame emitted on a new wall↔media map fixes the anchor
//! `(its media time, the boundary it is DUE at)` — the first grid boundary at
//! or after `wall_start + pts`, NOT the boundary it happened to be emitted at.
//! The audio block at any boundary `B` must then start at
//! `anchor_media + (B − anchor_wall)` in samples. That is the wall line the
//! picture follows (`present = wall_start + pts`), including the frame's
//! sub-slot phase. It continues across 24/25→30 repeat boundaries, so
//! correctly paired input is never corrected. A STALE first frame (a slow
//! decoder at song start, a stall) cannot pull the audio off the line: the
//! picture catches up to the line by dropping frames, and so does the audio.
//!
//! - **Hard alignment** — silence until the expected media time is buffered,
//!   then drop early / pad late audio so the first non-silent block starts at
//!   the expected media time ±1 sample
//!   ([`AudioGridBuffer::align_to`](crate::playback::audio_grid::AudioGridBuffer::align_to)).
//!   A NEW map (`anchor` = play/seek/new song, a lag re-anchor that moves
//!   `wall_start`) forgets the anchor ([`AvAlign::realign`]). An event that
//!   keeps the map (a grid resync, Resume) keeps the anchor and only re-snaps
//!   the buffer onto it ([`AvAlign::resnap`]).
//! - **Continuous correction** (every later productive boundary): past 5 ms of
//!   error drop/insert at most 48 samples per block until ≤ 1 ms
//!   ([`correction_for`]). This is the ONLY controller on the buffer — the old
//!   PLL level trim is gone.
//!
//! Audio enters the buffer when a frame is PULLED ([`Pacer::pull_frame`]),
//! not when it is consumed: the aligned take at boundary `B` needs media up to
//! `B + 33 ms`, which only the NEXT (parked) frame's paired audio covers.
//!
//! **The read-ahead cushion (#148 v4).** The paced decoder is opened with
//! [`open_paced_decoder`], so each frame carries audio up to its pts +
//! [`PACED_AUDIO_LEAD_MS`] (not the 40 ms pairing deadline). The buffer then
//! holds about that much media beyond the boundary being taken, so a video
//! decode stall of up to ~the lead (no pull for several boundaries) keeps
//! playing real audio instead of zero-filling an underrun. The depth has no
//! effect on A/V: the take is aligned by the media head, never by the level.
//!
//! Split into a sibling of `pacer.rs` so that file stays under the 1000-line
//! cap; as a child module it reaches the `Pacer`'s private state directly.

use super::{AUDIO_GRID_RATE_HZ, PacedFrame, Pacer};
use crate::playback::audio_grid::{correction_for, samples_from_100ns};
use sp_core::genlock::strict_next_boundary_100ns;
use sp_decoder::{AudioStream, DecoderError, SplitSyncedDecoder, VideoStream};
use sp_ndi::AudioFrame;

/// How far ahead of each video frame the PACED decoder reads audio (#148 v4).
///
/// A stall longer than the cushion still underruns.
///
/// - **Size.** 250 ms covers ~7 boundaries of video stall, including the
///   measured MF stalls under a resident heavy child (#147).
/// - **Memory.** It stays far under the grid buffer's 2 s cap, even with the
///   one extra chunk the G5 read gate allows.
/// - **Fader latency.** It costs up to this much: `StemMixReader` applies the
///   gains at READ time (`karaoke-stems.md` G5).
///
/// The pacing-OFF path keeps its own pairing deadline
/// (`audio_emitter::decoder_tolerance_ms`).
pub const PACED_AUDIO_LEAD_MS: u64 = 250;

/// Open the split A/V decoder for the PACED pipeline: audio is read
/// [`PACED_AUDIO_LEAD_MS`] ahead of each video frame (#148 v4). The audio source
/// may be a plain mix or a `StemMixReader` — it is wrapped the same way.
pub fn open_paced_decoder(
    video: Box<dyn VideoStream>,
    audio: Box<dyn AudioStream>,
) -> Result<SplitSyncedDecoder, DecoderError> {
    SplitSyncedDecoder::with_audio_lead(video, audio, PACED_AUDIO_LEAD_MS)
}

/// Alignment state + telemetry for the paced audio (#148 design v2). The
/// default is "re-align pending", so a fresh pacer aligns on its first frame.
#[derive(Debug, Default)]
pub(super) struct AvAlign {
    /// `(media sample, due boundary 100 ns)` of the frame the audio is
    /// anchored to; `None` = waiting for a fresh frame on a new map.
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
    /// A NEW wall↔media map (play/seek/new song, a lag re-anchor): forget the
    /// anchor; the next fresh frame fixes it and the audio hard-aligns to it.
    pub(super) fn realign(&mut self) {
        self.anchor = None;
        self.resnap();
    }

    /// The SAME map, but the buffer lost its place (a grid resync skipped
    /// boundaries; Resume flushed the audio): keep the anchor and hard-align
    /// the buffer back onto its line on the next productive boundary.
    pub(super) fn resnap(&mut self) {
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
            // The boundary the frame is DUE at (first grid boundary at or after
            // its presentation time), never the one it was emitted at.
            let (wall_start, fps) = (self.wall_start_100ns, self.grid_fps);
            let due = |pts: i64| strict_next_boundary_100ns(wall_start + pts - 1, fps);
            self.av.anchor =
                fresh_pts_100ns.map(|pts| (samples_from_100ns(pts, AUDIO_GRID_RATE_HZ), due(pts)));
        }
        let Some((media, wall)) = self.av.anchor else {
            // New map, no fresh frame yet (a repeat boundary): silence.
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
