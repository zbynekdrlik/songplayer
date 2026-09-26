//! Media-time A/V alignment of the PACED audio (#148 design v2).
//!
//! The pacer emits the video frame and its boundary audio block in the SAME
//! [`service`](super::Pacer::service) call, so the anchor is local: the first
//! FRESH frame emitted on a new wall↔media map fixes the anchor
//! `(its media time, the boundary it is DUE at)` — the first grid boundary at
//! or after `wall_start + pts`, NOT the boundary it happened to be emitted at.
//! The audio block at any boundary `B` must then start at
//! `anchor_media + (B − anchor_wall)` in samples. It continues across
//! 24/25→30 repeat boundaries, so correctly paired input is never corrected. A
//! STALE first frame (a slow decoder at song start, a stall) cannot pull the
//! audio off the line: the picture catches up to the line by dropping frames,
//! and so does the audio.
//!
//! **The picture origin lands on the grid (#148 v6, Approach 3).** When that
//! frame fixes the anchor the pacer also moves `wall_start` to
//! `due(pts₀) − pts₀` ([`Pacer::land_origin_on_grid`], the lag re-anchor's own
//! rule), so the frame presents exactly on its due boundary and is still shown
//! there. Every frame then presents at `due + (pts − pts₀)`, the SAME line the
//! audio anchor runs on. Without it, an off-grid first frame (a start position
//! or seek lands anywhere inside a frame) kept `wall_start + pts` up to one slot
//! BEFORE the audio's due line, so 24/25-fps content showed each frame up to
//! 33 ms before its audio for the whole song (box: SP-slow `av_frame_offset` min
//! −25.7 ms), while 30-fps content, shown at its due boundaries, read 0.
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
//! **The emitted relation (#148 v5, [`AvFrameOffset`]).** Every productive
//! boundary that hands timed audio to the sink also records
//! `audio_block_media_start − emitted_frame_pts` (ms): the media time of the
//! block's first sample minus the pts of the frame handed with it (a repeat
//! uses the repeated frame). Measurement only — it never feeds the alignment.
//!
//! Split into a sibling of `pacer.rs` so that file stays under the 1000-line
//! cap; as a child module it reaches the `Pacer`'s private state directly.

use super::{AUDIO_GRID_RATE_HZ, PacedFrame, Pacer};
use crate::playback::audio_grid::{correction_for, samples_from_100ns};
use sp_core::genlock::{UNITS_PER_SECOND, strict_next_boundary_100ns};
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
    /// anchored to; `None` = waiting for a fresh frame on a new map. The
    /// picture origin lands on that same frame, so the due boundary is also its
    /// present time (#148 v6).
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
    /// The emitted audio-block − frame-pts relation, per UTC minute (#148 v5).
    pub(super) frame_offset: AvFrameOffset,
    /// The song's EOS audio tail, held for the NEXT standby boundary (#147): it
    /// leaves as that boundary's one audio block, next to the held last frame,
    /// never as an audio-only send. Forgotten on a new map.
    standby_tail: Option<Vec<AudioFrame>>,
}

impl AvAlign {
    /// A NEW wall↔media map (play/seek/new song, a lag re-anchor): forget the
    /// anchor; the next fresh frame fixes it and the audio hard-aligns to it.
    /// A held EOS tail belongs to the old map and is dropped with it.
    pub(super) fn realign(&mut self) {
        self.anchor = None;
        self.standby_tail = None;
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

/// One UTC minute (100-ns units).
const MINUTE_100NS: i64 = 60 * UNITS_PER_SECOND;

/// `audio_block_media_start − emitted_frame_pts` in milliseconds: the block's
/// first sample (a media sample index at 48 kHz) minus the frame pts (100 ns).
/// Positive = the audio handed to NDI is from LATER media than the picture
/// handed with it, i.e. the audio LEADS (the sign of `av_align_err_ms` and the
/// gate).
fn frame_offset_ms(block_media: i64, frame_pts_100ns: i64) -> f64 {
    block_media as f64 * 1000.0 / AUDIO_GRID_RATE_HZ as f64 - frame_pts_100ns as f64 / 10_000.0
}

/// Mean/min/max of the `av_frame_offset` readings of one UTC minute.
#[derive(Clone, Copy, Debug, Default)]
struct OffsetWindow {
    /// UTC minute index (`stamp / 60 s`) the readings belong to.
    minute: i64,
    n: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl OffsetWindow {
    fn add(&mut self, v: f64) {
        if self.n == 0 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.n += 1;
        self.sum += v;
    }

    fn mean(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum / self.n as f64
        }
    }
}

/// The emitted A/V relation of the paced output (#148 v5), windowed per UTC
/// minute of the boundary stamp — the same minute bucket the engine's
/// once-per-minute `ndi: genlock` line uses. The pacer cannot see when that
/// line is logged, so it reports the last COMPLETE minute: the line logged in
/// minute M carries the whole of minute M−1.
#[derive(Debug, Default)]
pub(super) struct AvFrameOffset {
    /// The minute being filled.
    cur: OffsetWindow,
    /// The minute before `cur` that had readings.
    done: OffsetWindow,
}

impl AvFrameOffset {
    /// Add one boundary's reading, stamped `stamp_100ns` (UTC 100 ns). A stamp
    /// in a later minute closes the current window first.
    fn record(&mut self, stamp_100ns: i64, offset_ms: f64) {
        let minute = stamp_100ns.div_euclid(MINUTE_100NS);
        if minute > self.cur.minute {
            self.done = self.cur;
            self.cur = OffsetWindow {
                minute,
                ..OffsetWindow::default()
            };
        }
        self.cur.add(offset_ms);
    }

    /// `(mean, min, max)` in ms of the minute before `now_100ns`'s minute;
    /// all 0 when that minute had no reading (idle, paused, untimed audio).
    pub(super) fn report(&self, now_100ns: i64) -> (f64, f64, f64) {
        let w = self.window_before(now_100ns);
        (w.mean(), w.min, w.max)
    }

    /// Readings in the window [`report`](Self::report) covers — lets a test
    /// tell a measured 0 from an empty minute.
    #[cfg(test)]
    pub(super) fn readings(&self, now_100ns: i64) -> u64 {
        self.window_before(now_100ns).n
    }

    /// The window of the minute before `now_100ns`'s minute (empty if none).
    fn window_before(&self, now_100ns: i64) -> OffsetWindow {
        let want = now_100ns.div_euclid(MINUTE_100NS) - 1;
        if self.cur.minute == want {
            self.cur
        } else if self.done.minute == want {
            self.done
        } else {
            OffsetWindow::default()
        }
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
    /// Land a NEW map's picture origin on the grid (#148 v6, Approach 3) and
    /// return the boundary the frame at `pts` is DUE at (the first grid
    /// boundary at or after its present time `wall_start + pts`, never the one
    /// it happened to be emitted at). `wall_start` becomes `due − pts`, the lag
    /// re-anchor's rule, so the frame presents exactly on `due`. It was picked
    /// for a boundary ≥ `due`, so it is still shown there; every later frame
    /// presents `due − (old wall_start + pts)` (< one slot) later. Called only
    /// when the audio anchor is fixed, i.e. on the first fresh frame of a map
    /// that has timed audio; a resnap (grid resync, Resume) keeps the anchor,
    /// the map and so the origin.
    pub(super) fn land_origin_on_grid(&mut self, pts: i64) -> i64 {
        let due = strict_next_boundary_100ns(self.wall_start_100ns + pts - 1, self.grid_fps);
        self.wall_start_100ns = due - pts;
        due
    }

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
    /// boundary (`None` on a repeat); `shown_pts_100ns` is the pts of the frame
    /// handed to the sink with the block (fresh or repeated). Untimed audio (no
    /// media head) plays as a plain FIFO; timed audio is hard-aligned after a
    /// trigger, then corrected continuously. Every timed take once aligned
    /// records its block start against `shown_pts_100ns` (#148 v5); a fully
    /// underrun (zero-filled) block records the head it resumes from, so a
    /// cushion-exhausting stall shows as a negative reading.
    pub(super) fn take_aligned_audio(
        &mut self,
        stamp_100ns: i64,
        fresh_pts_100ns: Option<i64>,
        shown_pts_100ns: i64,
    ) -> Vec<AudioFrame> {
        let n = self.samples_per_boundary;
        let Some(head) = self.audio_buf.head_media() else {
            return interleave(self.audio_buf.take_boundary_chunk(n));
        };
        if self.av.anchor.is_none()
            && let Some(pts) = fresh_pts_100ns
        {
            // Anchor on the frame's DUE boundary and land the picture origin on
            // it, so picture and audio run on ONE line (#148 v6).
            let due = self.land_origin_on_grid(pts);
            self.av.anchor = Some((samples_from_100ns(pts, AUDIO_GRID_RATE_HZ), due));
        }
        let Some((media, wall)) = self.av.anchor else {
            // New map, no fresh frame yet (a repeat boundary): silence.
            return interleave(vec![vec![0.0; n]; self.audio_buf.channels()]);
        };
        let expected = media + samples_from_100ns(stamp_100ns - wall, AUDIO_GRID_RATE_HZ);
        self.av.last_err = head - expected;
        if self.av.aligned {
            // The block starts at the head (output 0 = input 0, also on an underrun).
            let offset = frame_offset_ms(head, shown_pts_100ns);
            self.av.frame_offset.record(stamp_100ns, offset);
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
            // Aligned: the head (the block's first sample) is now `expected`.
            let offset = frame_offset_ms(expected, shown_pts_100ns);
            self.av.frame_offset.record(stamp_100ns, offset);
            interleave(self.audio_buf.take_boundary_chunk(n))
        } else {
            // Not enough audio buffered to drop yet: silence, retry next boundary.
            interleave(vec![vec![0.0; n]; self.audio_buf.channels()])
        }
    }

    /// The ONE audio block every STANDBY boundary carries (idle black and paused
    /// frozen frame, #147 standby same-path, design comment 5841796900). A
    /// standby boundary then hands the sink the same audio-then-video pair as a
    /// playing boundary, so the receiver sees one constant A/V cadence idle or
    /// playing, and its audio hold never has to re-engage at a song start.
    ///
    /// It is the song's EOS tail when one is held
    /// ([`hold_eos_tail_for_standby`](Self::hold_eos_tail_for_standby)), once;
    /// otherwise `samples_per_boundary` zeros per channel, the same shape as the
    /// playing path's silence above. The layout follows the song when the grid
    /// buffer knows it (paused, or just after a song ended), else stereo. Every
    /// playlist file decodes to stereo, so the layout does not flip in practice.
    pub(super) fn standby_block(&mut self) -> Vec<AudioFrame> {
        if let Some(tail) = self.av.standby_tail.take() {
            return tail;
        }
        let channels = match self.audio_buf.channels() {
            0 => STANDBY_SILENCE_CHANNELS,
            n => n,
        };
        vec![AudioFrame {
            data: vec![0.0; self.samples_per_boundary * channels],
            channels: channels as u32,
            sample_rate: AUDIO_GRID_RATE_HZ,
            timecode_100ns: None,
        }]
    }

    /// Hold the song's EOS audio tail ([`take_eos_tail`](Pacer::take_eos_tail),
    /// the last partial boundary zero-filled, #148 rework item 4) for the NEXT
    /// standby boundary (#147). At a natural song end the paced pipeline then
    /// services one more boundary with the held last frame, so the tail leaves
    /// as that boundary's one audio block instead of an audio-only send, and the
    /// receiver keeps exactly one audio block per video boundary into the idle
    /// fill. Nothing is held when nothing was buffered. A new map
    /// ([`anchor`](Pacer::anchor), a lag re-anchor) drops a held tail.
    pub fn hold_eos_tail_for_standby(&mut self) {
        let tail = self.take_eos_tail();
        self.av.standby_tail = tail.is_empty().then_some(tail);
    }
}

/// Channel count of the standby silence while no song has fixed one (#147):
/// stereo, the layout every playlist file decodes to.
const STANDBY_SILENCE_CHANNELS: usize = 2;
