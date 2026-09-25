//! Media-time A/V alignment of the PACED audio (#148 design v2) — acceptance.
//!
//! Drives the pure [`Pacer`] over a settable wall clock with audio whose sample
//! VALUES encode their own media sample index (`enc(s) = s + 1_000_000`, exact
//! in `f32`; silence = `0.0`), so every assertion reads the media time the
//! pacer actually put on the wire at each boundary:
//!
//! * the first non-silent block after start / seek starts on the picture's
//!   line, media `boundary − wall_start` (±1 sample; = the emitted frame's
//!   media time when it presents on the grid, #148 v6) — early audio is
//!   dropped, late audio is padded with silence;
//! * nothing is emitted before the first frame;
//! * a 24-fps source on the 30-fps grid stays within 1 ms for 60 s;
//! * a 20 ms error (a decoder stall) converges by ≤ 48 samples per block and
//!   stops at ≤ 1 ms;
//! * aligned input is never corrected, and a deep buffer is never servoed
//!   toward a level target (no PLL level trim remains).
//!
//! Frames are built like `pipeline_paced::to_paced_frame` builds them: each
//! audio chunk carries its 0-based media time in `timecode_100ns`.
//! `super::super::*` resolves to the `pacer` module under test.

use super::super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;
use std::cell::{Cell, RefCell};

const RATE: i64 = 48_000;
const SPB: i64 = 1600;
/// Encoding offset so media sample 0 is not confused with silence.
const ENC_BIAS: i64 = 1_000_000;
/// The split-sync pairing tolerance (`DEFAULT_TOLERANCE_MS` = 40 ms) in samples.
const PAIR_TOLERANCE_SAMPLES: i64 = 1920;
/// A typical decoded audio chunk (48 ms).
const CHUNK: i64 = 2304;

/// The k-th exact-rational 30-fps grid boundary (100-ns units).
fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

fn enc(s: i64) -> f32 {
    (s + ENC_BIAS) as f32
}

/// The media sample index a sample value encodes; `None` for silence.
fn dec(v: f32) -> Option<i64> {
    if v == 0.0 {
        None
    } else {
        Some(v as i64 - ENC_BIAS)
    }
}

/// 100-ns → samples, rounded to the nearest sample.
fn samples_of(t_100ns: i64) -> i64 {
    (t_100ns * RATE * 2 + 10_000_000).div_euclid(20_000_000)
}

/// Samples → 100-ns, rounded (the media timecode a chunk carries).
fn tc_of(samples: i64) -> i64 {
    (samples * 10_000_000 * 2 + RATE).div_euclid(2 * RATE)
}

/// A stereo chunk of media samples `[start, start + len)` (ch1 = −ch0).
fn chunk(start: i64, len: i64) -> AudioFrame {
    let mut data = Vec::with_capacity((len * 2) as usize);
    for s in start..start + len {
        data.push(enc(s));
        data.push(-enc(s));
    }
    AudioFrame {
        data,
        channels: 2,
        sample_rate: 48_000,
        timecode_100ns: Some(tc_of(start)),
    }
}

fn frame(pts_100ns: i64, audio: Vec<AudioFrame>) -> PacedFrame {
    PacedFrame {
        pts_ns: pts_100ns * 100,
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![0u8; 12]),
        audio,
    }
}

/// A split-sync-like source: video frame `j` at `pts(j)`; contiguous audio from
/// `audio_start` in `CHUNK`-sample chunks, each chunk paired with the first
/// frame whose `pts + 40 ms` reaches the chunk start (`SplitSyncedDecoder`).
struct SyncedSource {
    pts: fn(i64) -> i64,
    audio_start: i64,
    next_frame: i64,
    next_chunk: i64,
}

impl SyncedSource {
    fn new(pts: fn(i64) -> i64, audio_start: i64) -> Self {
        Self {
            pts,
            audio_start,
            next_frame: 0,
            next_chunk: 0,
        }
    }

    fn next(&mut self) -> PacedFrame {
        let pts = (self.pts)(self.next_frame);
        self.next_frame += 1;
        let deadline = samples_of(pts) + PAIR_TOLERANCE_SAMPLES;
        let mut audio = Vec::new();
        loop {
            let start = self.audio_start + self.next_chunk * CHUNK;
            if start > deadline {
                break;
            }
            audio.push(chunk(start, CHUNK));
            self.next_chunk += 1;
        }
        frame(pts, audio)
    }
}

/// 30-fps source, frame `j` at `b(j)`, carrying media
/// `[j·1600 + ahead, (j+1)·1600 + ahead)` — frame 0 also carries `[0, ahead)`,
/// so the audio stream is contiguous from 0 and runs `ahead` samples in front
/// of the video.
fn ahead_frame(j: i64, ahead: i64) -> PacedFrame {
    let a = if j == 0 {
        chunk(0, SPB + ahead)
    } else {
        chunk(j * SPB + ahead, SPB)
    };
    frame(b(j), vec![a])
}

/// Records, per emitted boundary, the video stamp and channel 0 of the audio.
#[derive(Default)]
struct Rec {
    blocks: Vec<(i64, Vec<f32>)>,
}

impl PacedSink for Rec {
    fn emit(
        &mut self,
        _video: &PacedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        _audio_tc_100ns: i64,
    ) {
        let mut ch0 = Vec::new();
        for a in audio {
            let c = a.channels as usize;
            if c == 0 {
                continue;
            }
            for j in 0..a.data.len() / c {
                ch0.push(a.data[j * c]);
            }
        }
        self.blocks.push((video_tc_100ns, ch0));
    }
}

fn anchored() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

/// Service boundaries `from..=to` pulling from `src`.
fn run_synced(
    pacer: &mut Pacer,
    clk: &SettableClock,
    src: &RefCell<SyncedSource>,
    from: i64,
    to: i64,
    rec: &mut Rec,
) {
    for k in from..=to {
        clk.set(b(k));
        pacer.service(|| Some(src.borrow_mut().next()), rec);
    }
}

/// The media sample of a block's first sample (panics on silence).
fn first_media(block: &[f32]) -> i64 {
    dec(block[0]).expect("block starts with real audio")
}

/// Assert every sample of every block is the exact consecutive media sample
/// `first + i·1600 + s` — no correction, no interpolation, no silence.
fn assert_exact_stream(blocks: &[(i64, Vec<f32>)], first: i64) {
    for (i, (_tc, blk)) in blocks.iter().enumerate() {
        assert_eq!(blk.len(), SPB as usize, "block {i} has a full boundary");
        for (s, &v) in blk.iter().enumerate() {
            let want = first + i as i64 * SPB + s as i64;
            assert_eq!(
                dec(v),
                Some(want),
                "block {i} sample {s}: media {:?}, want {want}",
                dec(v)
            );
        }
    }
}

#[test]
fn a_180_ms_start_skew_aligns_the_first_block_to_the_picture_line() {
    // Video starts 180 ms into the media; audio is decoded from media 0. The
    // first frame PRESENTS at wall_start + 180 ms (b(1) + 180 ms, 20 ms before
    // b(7)) and is shown at the 200 ms boundary b(7). The audio runs on the
    // picture's line (#148 v6): the block at b(7) starts at media
    // b(7) − wall_start = 200 ms = 9600, not at the audio's start and not at
    // the frame's own 180 ms (that would run the audio 20 ms late for the song).
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(|j| 1_800_000 + b(j), 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, 40, &mut rec);

    assert_eq!(
        rec.blocks[0].0,
        b(7),
        "first frame emitted at the 200 ms boundary"
    );
    let m0 = first_media(&rec.blocks[0].1);
    assert!(
        (m0 - 9600).abs() <= 1,
        "first block starts at media {m0}, want the picture line's 9600 (±1 sample)"
    );
    assert_exact_stream(&rec.blocks, 9600);
}

#[test]
fn no_audio_is_submitted_before_the_first_frame_and_pre_frame_media_never_plays() {
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(|j| 1_800_000 + b(j), 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, 40, &mut rec);

    // b(1)..b(6) are pre-roll (no frame yet): nothing is submitted at all.
    assert_eq!(rec.blocks.len(), 34, "only b(7)..=b(40) emit");
    let min_media = rec
        .blocks
        .iter()
        .flat_map(|(_, blk)| blk.iter().filter_map(|&v| dec(v)))
        .min()
        .expect("audio was emitted");
    assert!(
        min_media >= 8639,
        "media before the first frame (180 ms) must never play, got {min_media}"
    );
}

#[test]
fn late_audio_is_padded_with_silence_then_plays_aligned() {
    // Video from media 0, audio only from media 50 ms (2400). The first block
    // at b(1) is silence; the first real sample lands at wall offset 2400 —
    // exactly its media time — and every later sample stays aligned.
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(b, 2400));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, 30, &mut rec);

    let b0 = rec.blocks[0].0;
    assert_eq!(b0, b(1), "frame 0 is emitted at the anchor boundary");
    let mut first_real: Option<i64> = None;
    for (tc, blk) in &rec.blocks {
        assert_eq!(
            blk.len(),
            SPB as usize,
            "every productive boundary carries a full block"
        );
        let base = samples_of(tc - b0);
        for (s, &v) in blk.iter().enumerate() {
            let wall_idx = base + s as i64;
            match dec(v) {
                Some(m) => {
                    assert_eq!(
                        m, wall_idx,
                        "real sample at wall offset {wall_idx} has media {m}"
                    );
                    first_real.get_or_insert(wall_idx);
                }
                None => assert!(
                    wall_idx < 2400,
                    "silence at wall offset {wall_idx} after the audio started"
                ),
            }
        }
    }
    assert_eq!(
        first_real,
        Some(2400),
        "the first real sample plays at its media time"
    );
}

#[test]
fn a_seek_realigns_to_the_new_frame_media_time() {
    let (mut pacer, clk) = anchored();
    let before = RefCell::new(SyncedSource::new(b, 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &before, 1, 30, &mut rec);
    assert_exact_stream(&rec.blocks, 0);

    // Seek: the producer now delivers the new position — audio from the seek
    // target (media 0), the first video frame 100 ms later (keyframe landing).
    clk.set(b(30) + 100);
    pacer.anchor();
    let after = RefCell::new(SyncedSource::new(|j| 1_000_000 + b(j), 0));
    let mut rec2 = Rec::default();
    run_synced(&mut pacer, &clk, &after, 31, 60, &mut rec2);

    assert_eq!(
        rec2.blocks[0].0,
        b(34),
        "first post-seek frame due at b(34)"
    );
    let m0 = first_media(&rec2.blocks[0].1);
    assert!(
        (m0 - 4800).abs() <= 1,
        "post-seek first block starts at media {m0}, want the frame's 4800 (±1)"
    );
    assert_exact_stream(&rec2.blocks, 4800);
}

#[test]
fn a_24_fps_source_on_the_30_fps_grid_stays_within_1_ms_for_60_s() {
    // 23.976/24-fps content: some 30-fps boundaries only REPEAT the picture and
    // bring no new audio, yet each boundary takes 1600 samples. Integer-ms PTS
    // like the decoder reports them. Over 60 s every block's first sample must
    // sit within 1 ms (48 samples) of the grid media time, with no silence.
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(|j| (j * 1000 / 24) * 10_000, 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, 1801, &mut rec);

    assert_eq!(
        rec.blocks.len(),
        1801,
        "every boundary emits (frame or repeat)"
    );
    for (i, (_tc, blk)) in rec.blocks.iter().enumerate() {
        assert_eq!(blk.len(), SPB as usize, "block {i} is a full boundary");
        assert!(
            blk.iter().all(|&v| v != 0.0),
            "block {i} contains silence (an underrun) on a steadily fed source"
        );
        let err = first_media(blk) - i as i64 * SPB;
        assert!(err.abs() <= 48, "block {i}: A/V error {err} samples > 1 ms");
    }
    // In fact bit-exact: correctly paired input never needs a correction.
    assert_exact_stream(&rec.blocks, 0);
}

#[test]
fn a_20_ms_error_converges_at_most_48_samples_per_block_and_stops_at_1_ms() {
    // Audio runs 640 samples ahead of the video (frame j carries media
    // [j·1600+640, (j+1)·1600+640)). A 2-call decoder stall (b(51), b(52))
    // leaves only 640 samples for the b(52) block: 960 samples (20 ms) are
    // zero-filled, so from b(53) on the audio is 20 ms behind the picture.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let stalled = Cell::new(false);
    let mut rec = Rec::default();
    for k in 1..=150i64 {
        clk.set(b(k));
        stalled.set(k == 51 || k == 52);
        pacer.service(
            || {
                if stalled.get() {
                    return None;
                }
                let j = next.get();
                next.set(j + 1);
                Some(ahead_frame(j, 640))
            },
            &mut rec,
        );
    }
    assert_eq!(rec.blocks.len(), 150);
    let errs: Vec<i64> = rec
        .blocks
        .iter()
        .enumerate()
        .map(|(i, (_, blk))| first_media(blk) - i as i64 * SPB)
        .collect();
    for (i, e) in errs.iter().enumerate().take(52) {
        assert_eq!(*e, 0, "block {i} before the stall is aligned");
    }
    let starved = &rec.blocks[51].1;
    assert_eq!(
        starved.iter().filter(|&&v| v == 0.0).count(),
        960,
        "the stalled boundary zero-fills 960 samples"
    );
    assert_eq!(errs[52], -960, "the stall leaves the audio 20 ms behind");
    // Converge by exactly 48 samples per block: -960 → -912 → … → -48.
    for (i, w) in errs.windows(2).enumerate().skip(52).take(19) {
        assert_eq!(
            w[1] - w[0],
            48,
            "block {}: the correction step is 48 samples (err {} → {})",
            i + 1,
            w[0],
            w[1]
        );
    }
    assert_eq!(errs[71], -48, "converged to 1 ms");
    for (i, e) in errs.iter().enumerate().skip(71) {
        assert_eq!(
            *e, -48,
            "block {i}: no further correction once |err| ≤ 1 ms"
        );
    }
    for (i, (_, blk)) in rec.blocks.iter().enumerate().skip(52) {
        assert!(
            blk.iter().all(|&v| v != 0.0),
            "block {i}: the correction never inserts silence"
        );
    }
}

#[test]
fn aligned_input_is_never_corrected() {
    // Audio exactly paired with 30-fps video: 60 s of boundaries play the
    // media samples bit-exact and consecutive.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=1800i64 {
        clk.set(b(k));
        pacer.service(
            || {
                let j = next.get();
                next.set(j + 1);
                Some(ahead_frame(j, 0))
            },
            &mut rec,
        );
    }
    assert_eq!(rec.blocks.len(), 1800);
    assert_exact_stream(&rec.blocks, 0);
}

#[test]
fn a_deep_buffer_is_never_servoed_toward_a_level_target() {
    // Audio decoded a full second ahead of the video keeps the buffer ~1 s deep.
    // The media head is aligned, so nothing may touch the read: no PLL level
    // trim remains that would resample toward a fixed level (the old trim
    // engaged after 60–120 s beyond ±2 boundaries). 130 s of boundaries play
    // the media samples bit-exact.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=3900i64 {
        clk.set(b(k));
        pacer.service(
            || {
                let j = next.get();
                next.set(j + 1);
                Some(ahead_frame(j, RATE))
            },
            &mut rec,
        );
    }
    assert_eq!(rec.blocks.len(), 3900);
    assert_exact_stream(&rec.blocks, 0);
}

// ---------------------------------------------------------------------------
// Re-align triggers + telemetry (#148 design v2): every change of the
// wall↔media map re-anchors the audio on the next fresh frame, and the
// alignment is reported on `PacingStats`.
// ---------------------------------------------------------------------------

/// Drive one exact-source service call at `now`, stalling the pull when asked.
fn step_ahead(
    pacer: &mut Pacer,
    clk: &SettableClock,
    now: i64,
    next: &Cell<i64>,
    stall: bool,
    ahead: i64,
    rec: &mut Rec,
) -> ServiceOutcome {
    clk.set(now);
    pacer.service(
        || {
            if stall {
                return None;
            }
            let j = next.get();
            next.set(j + 1);
            Some(ahead_frame(j, ahead))
        },
        rec,
    )
}

#[test]
fn the_20_ms_convergence_is_reported_on_pacing_stats() {
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=150i64 {
        let stall = k == 51 || k == 52;
        step_ahead(&mut pacer, &clk, b(k), &next, stall, 640, &mut rec);
    }
    let s = pacer.stats();
    // 19 blocks of 48 samples: -960 → -48.
    assert_eq!(s.av_corrections, 19, "one correction per converging block");
    assert_eq!(s.av_corrected_samples, 912, "19 × 48 samples dropped");
    assert_eq!(
        s.av_align_err_ms, -1.0,
        "the residual 48-sample error is 1 ms"
    );
    assert_eq!(
        pacer.audio_stats().underruns,
        1,
        "the stall is one underrun"
    );
}

#[test]
fn the_start_alignment_is_counted_and_then_reads_zero_error() {
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(|j| 1_800_000 + b(j), 0));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, 7, &mut rec);
    let s = pacer.stats();
    assert_eq!(s.av_corrections, 1, "the start drop is one correction");
    // The picture line at b(7) is media b(7) − wall_start = 200 ms (#148 v6).
    assert_eq!(
        s.av_corrected_samples, 9600,
        "200 ms of early audio dropped"
    );
    assert_eq!(s.av_align_err_ms, -200.0, "measured before the drop");
    run_synced(&mut pacer, &clk, &src, 8, 12, &mut rec);
    assert_eq!(pacer.stats().av_align_err_ms, 0.0, "aligned afterwards");
    assert_eq!(pacer.stats().av_corrections, 1, "no further correction");
}

#[test]
fn padding_late_audio_is_counted_as_one_correction() {
    let (mut pacer, clk) = anchored();
    let src = RefCell::new(SyncedSource::new(b, 2400));
    let mut rec = Rec::default();
    run_synced(&mut pacer, &clk, &src, 1, 10, &mut rec);
    let s = pacer.stats();
    assert_eq!(s.av_corrections, 1);
    assert_eq!(s.av_corrected_samples, 2400, "50 ms of silence padded");
    assert_eq!(s.av_align_err_ms, 0.0);
}

#[test]
fn a_lag_reanchor_realigns_the_audio_to_the_resumed_frame() {
    // The pacer re-anchors `wall_start` after > 1 s of lag with a frame
    // buffered (#147 lane 3). The audio must follow the moved wall↔media map:
    // it re-anchors on the resumed frame instead of chasing the old line.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    // 19 slots late: frame 0 is emitted (stamped b(1)), the lag timer arms.
    let o1 = step_ahead(&mut pacer, &clk, b(20), &next, false, 0, &mut rec);
    assert_eq!(o1, ServiceOutcome::Emitted);
    // Still behind > 1 s later → re-anchor onto the parked frame 1.
    let late = b(20) + 10_000_001;
    let until = match step_ahead(&mut pacer, &clk, late, &next, false, 0, &mut rec) {
        ServiceOutcome::Reanchored { until_100ns, .. } => until_100ns,
        other => panic!("expected Reanchored, got {other:?}"),
    };
    for k in 0..10i64 {
        step_ahead(&mut pacer, &clk, until + b(k), &next, false, 0, &mut rec);
    }
    // Frame 1 (media 1600) and its successors play bit-exact from `until` on.
    assert_eq!(rec.blocks.len(), 11);
    assert_exact_stream(&rec.blocks, 0);
    assert_eq!(
        pacer.stats().av_corrections,
        0,
        "the re-anchor needs no drop/pad"
    );
}

#[test]
fn a_grid_resync_plays_silence_then_realigns_on_the_next_fresh_frame() {
    // Audio 640 samples ahead. At b(51) the decoder stalls (frame 50 is the last
    // one). The next call comes 12 slots late with still nothing decoded: the
    // gate RESYNCS (repeat stamped at b(63)). The map is unchanged, so the audio
    // re-snaps onto its kept anchor: at b(63) too little is buffered to reach
    // the wall line → one silent block; at b(64) the decoder catches up and the
    // audio plays the wall line (frame 63's media time) exactly.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=51i64 {
        step_ahead(&mut pacer, &clk, b(k), &next, k == 51, 640, &mut rec);
    }
    let resyncs_before = pacer.stats().resyncs;
    let o = step_ahead(&mut pacer, &clk, b(63), &next, true, 640, &mut rec);
    assert_eq!(o, ServiceOutcome::Repeated);
    assert_eq!(
        pacer.stats().resyncs,
        resyncs_before + 1,
        "the gate resynced"
    );
    let (tc, silent) = rec.blocks.last().unwrap();
    assert_eq!(*tc, b(63));
    assert_eq!(silent.len(), SPB as usize);
    assert!(
        silent.iter().all(|&v| v == 0.0),
        "re-align pending → silence"
    );

    for k in 64..=80i64 {
        step_ahead(&mut pacer, &clk, b(k), &next, false, 640, &mut rec);
    }
    let after = &rec.blocks[52..];
    assert_eq!(after[0].0, b(64));
    assert_exact_stream(after, 63 * SPB);
}

#[test]
fn audio_resume_reset_realigns_to_the_next_fresh_frame() {
    // Resume flushes the audio (frame 30's paired audio goes with it) and the
    // audio re-snaps onto its kept anchor: at b(31) the wall line needs media
    // 48000, which is gone, so that block is padded silence and frame 31's
    // audio plays exactly at its media time on the next boundary.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=30i64 {
        step_ahead(&mut pacer, &clk, b(k), &next, false, 0, &mut rec);
    }
    pacer.audio_resume_reset();
    for k in 31..=40i64 {
        step_ahead(&mut pacer, &clk, b(k), &next, false, 0, &mut rec);
    }
    let (_, padded) = &rec.blocks[30];
    assert!(
        padded.iter().all(|&v| v == 0.0),
        "frame 30's audio was flushed"
    );
    assert_exact_stream(&rec.blocks[31..], 31 * SPB);
    assert_eq!(pacer.stats().av_corrected_samples, 1600, "one block padded");
}

#[test]
fn untimed_audio_plays_as_a_plain_fifo() {
    // Audio with no media time (no head) is never aligned or corrected.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=20i64 {
        clk.set(b(k));
        pacer.service(
            || {
                let j = next.get();
                next.set(j + 1);
                let mut f = ahead_frame(j, 640);
                f.audio[0].timecode_100ns = None;
                Some(f)
            },
            &mut rec,
        );
    }
    assert_exact_stream(&rec.blocks, 0);
    assert_eq!(pacer.stats().av_corrections, 0);
    assert_eq!(pacer.stats().av_align_err_ms, 0.0);
}

/// Drive one exact-source service call at `now` that may pull at most `limit`
/// frames (a producer that has only that many decoded).
fn step_limited(
    pacer: &mut Pacer,
    clk: &SettableClock,
    now: i64,
    next: &Cell<i64>,
    limit: usize,
    ahead: i64,
    rec: &mut Rec,
) {
    clk.set(now);
    let pulled = Cell::new(0usize);
    pacer.service(
        || {
            if pulled.get() >= limit {
                return None;
            }
            pulled.set(pulled.get() + 1);
            let j = next.get();
            next.set(j + 1);
            Some(ahead_frame(j, ahead))
        },
        rec,
    );
}

#[test]
fn a_late_first_frame_does_not_leave_the_audio_behind_for_the_song() {
    // The decoder needs 3 boundaries for its first frame: frame 0 (due at
    // b(1)) is emitted STALE at b(4), and at b(5) the picture catches up to the
    // wall line (frames 1..4 arrive, older ones dropped). The audio must follow
    // the wall line — media (S − wall_start) — not the stale first frame, or it
    // stays 100 ms behind the picture for the whole song.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=3i64 {
        step_limited(&mut pacer, &clk, b(k), &next, 0, 640, &mut rec);
    }
    assert!(rec.blocks.is_empty(), "pre-roll: nothing emitted");
    step_limited(&mut pacer, &clk, b(4), &next, 1, 640, &mut rec);
    for k in 5..=20i64 {
        step_limited(&mut pacer, &clk, b(k), &next, usize::MAX, 640, &mut rec);
    }
    // b(4): the wall line needs media 4800, not decoded yet → silence.
    assert_eq!(rec.blocks[0].0, b(4));
    assert!(rec.blocks[0].1.iter().all(|&v| v == 0.0));
    // From b(5) on, every block plays the wall line bit-exact.
    assert_eq!(rec.blocks[1].0, b(5));
    assert_exact_stream(&rec.blocks[1..], 4 * SPB);
}

#[test]
fn a_grid_resync_realigns_to_the_wall_line_not_to_a_stale_frame() {
    // Decoder stall + a late call → the gate RESYNCS (b(63)). The decoder then
    // recovers over two calls: at b(64) only frame 51 (due at b(52)) is ready
    // and is emitted STALE; at b(65) the picture catches up to the wall line.
    // The audio must snap back to the wall line (the map did not move), not
    // anchor on the stale frame 51 and stay ~400 ms behind for the song.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=50i64 {
        step_limited(&mut pacer, &clk, b(k), &next, usize::MAX, 640, &mut rec);
    }
    // b(51): the parked frame 50 plays, nothing new is decoded.
    step_limited(&mut pacer, &clk, b(51), &next, 0, 640, &mut rec);
    let resyncs_before = pacer.stats().resyncs;
    step_limited(&mut pacer, &clk, b(63), &next, 0, 640, &mut rec);
    assert_eq!(
        pacer.stats().resyncs,
        resyncs_before + 1,
        "the gate resynced"
    );
    step_limited(&mut pacer, &clk, b(64), &next, 1, 640, &mut rec); // frame 51, stale
    for k in 65..=80i64 {
        step_limited(&mut pacer, &clk, b(k), &next, usize::MAX, 640, &mut rec);
    }
    let from = rec
        .blocks
        .iter()
        .position(|(tc, _)| *tc == b(65))
        .expect("b(65) emitted");
    assert_exact_stream(&rec.blocks[from..], 64 * SPB);
}

#[test]
fn audio_resume_reset_snaps_back_to_the_wall_line() {
    // Resume keeps the wall grid (and so the wall↔media map): after the flush
    // the audio re-aligns to the SAME line, whatever frame is emitted first.
    let (mut pacer, clk) = anchored();
    let next = Cell::new(0i64);
    let mut rec = Rec::default();
    for k in 1..=30i64 {
        step_limited(&mut pacer, &clk, b(k), &next, usize::MAX, 0, &mut rec);
    }
    pacer.audio_resume_reset();
    // After the resume the producer stalls for 3 calls, then delivers one
    // frame per call (frame 31, due at b(32), is emitted STALE at b(34)), then
    // catches up.
    for k in 31..=33i64 {
        step_limited(&mut pacer, &clk, b(k), &next, 0, 0, &mut rec);
    }
    for k in 34..=35i64 {
        step_limited(&mut pacer, &clk, b(k), &next, 1, 0, &mut rec);
    }
    for k in 36..=45i64 {
        step_limited(&mut pacer, &clk, b(k), &next, usize::MAX, 0, &mut rec);
    }
    let (_, last) = rec.blocks.last().unwrap();
    assert!(last.iter().all(|&v| v != 0.0), "re-aligned by the end");
    // Every non-silent sample after the resume sits on the wall line.
    for (tc, blk) in &rec.blocks[30..] {
        let base = samples_of(tc - b(1));
        for (s, &v) in blk.iter().enumerate() {
            if let Some(m) = dec(v) {
                assert_eq!(m, base + s as i64, "off the wall line at {tc}");
            }
        }
    }
}

// The paced read-ahead cushion end to end (#148 v4), nested here so it reuses
// the media-encoded helpers above.
#[path = "pacer_tests_av_lead.rs"]
mod pacer_tests_av_lead;

// #147: a preempted WallClock re-anchor must not relatch the pacer nor move
// the audio (the box's 0 → 6 relatches in one A/V take).
#[path = "pacer_tests_wall_anchor.rs"]
mod pacer_tests_wall_anchor;

// #148 v5: the per-minute `av_frame_offset` telemetry (SongPlayer's own
// emitted audio-block − frame-pts relation), on the same media-encoded helpers.
#[path = "pacer_tests_av_offset.rs"]
mod pacer_tests_av_offset;
