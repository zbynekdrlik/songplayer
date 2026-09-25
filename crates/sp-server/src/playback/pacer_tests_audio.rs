//! Pacer paced-audio wiring tests (#148).
//!
//! Drive the pure [`Pacer`] with a settable wall clock and assert the audio
//! contract: audio is decoupled from the video frame decision — the pacer pushes
//! every pulled frame's audio into its `AudioGridBuffer` and, at each active
//! boundary, submits EXACTLY `samples_per_boundary` (1600 @ 48 kHz/30) samples,
//! including on a sub-grid video repeat. These frames carry UNTIMED audio (a
//! plain FIFO); the media-time alignment is `pacer_tests_av_align.rs`.
//! `super::*` resolves to the `pacer` module under test.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

const B1: i64 = 333_333;

fn b(k: i64) -> i64 {
    k * 10_000_000 / 30
}

/// A 30-fps-grid frame due at `target_100ns` carrying `n` stereo audio samples.
fn frame_due_with_audio(target_100ns: i64, n: usize) -> PacedFrame {
    let pts_ns = (target_100ns - B1) * 100;
    // Stereo interleaved: alternate 0.25 / -0.25 so channel data is non-trivial.
    let mut data = Vec::with_capacity(n * 2);
    for _ in 0..n {
        data.push(0.25f32);
        data.push(-0.25f32);
    }
    PacedFrame {
        pts_ns,
        width: 4,
        height: 2,
        stride: 4,
        video: SharedFrame::new(vec![0u8; 12]),
        audio: vec![AudioFrame {
            data,
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
    }
}

/// Records, per emit, the audio timecode and the per-channel sample count of the
/// boundary batch the sink received.
#[derive(Default)]
struct AudioRecordingSink {
    video_tcs: Vec<i64>,
    audio_tcs: Vec<i64>,
    frames_per_emit: Vec<usize>,
    samples_per_emit: Vec<usize>,
}

impl PacedSink for AudioRecordingSink {
    fn emit(
        &mut self,
        _video: &PacedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        self.video_tcs.push(video_tc_100ns);
        self.audio_tcs.push(audio_tc_100ns);
        self.frames_per_emit.push(audio.len());
        let samples: usize = audio
            .iter()
            .map(|a| {
                if a.channels > 0 {
                    a.data.len() / a.channels as usize
                } else {
                    0
                }
            })
            .sum();
        self.samples_per_emit.push(samples);
    }
}

fn anchored_pacer() -> (Pacer, SettableClock) {
    let (wall, clk) = WallClock::settable(0);
    let mut pacer = Pacer::with_wallclock(30, true, wall);
    clk.set(0);
    pacer.anchor();
    (pacer, clk)
}

#[test]
fn each_boundary_submits_exactly_samples_per_boundary_from_the_buffer() {
    let (mut pacer, clk) = anchored_pacer();
    // 30-fps content: one frame per boundary, each carrying 1600 samples so the
    // grid consumption (1600/boundary) is balanced.
    let frames = RefCell::new(VecDeque::new());
    for j in 0..31i64 {
        frames
            .borrow_mut()
            .push_back(frame_due_with_audio(b(j + 1), 1600));
    }
    let mut sink = AudioRecordingSink::default();
    for k in 1..=30i64 {
        clk.set(b(k));
        pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    }

    assert!(!sink.samples_per_emit.is_empty(), "emitted boundaries");
    for (i, &s) in sink.samples_per_emit.iter().enumerate() {
        assert_eq!(s, 1600, "boundary {i} must submit exactly 1600 samples");
    }
    for (i, &f) in sink.frames_per_emit.iter().enumerate() {
        assert_eq!(f, 1, "boundary {i} must submit exactly one audio chunk");
    }
}

#[test]
fn a_sub_grid_video_repeat_still_submits_a_boundary_audio_chunk() {
    let (mut pacer, clk) = anchored_pacer();
    // 23.976-fps content: some boundaries have no due frame and REPEAT the video,
    // but audio is decoupled — the buffer keeps delivering 1600 samples/boundary.
    // Each source frame carries ~2002 samples (48000/23.976).
    let cap = 417_083i64;
    let frames = RefCell::new(VecDeque::new());
    for j in 0..40i64 {
        frames
            .borrow_mut()
            .push_back(frame_due_with_audio(B1 + j * cap, 2002));
    }
    let mut sink = AudioRecordingSink::default();
    let mut repeats = 0usize;
    for k in 1..=30i64 {
        clk.set(b(k));
        let outcome = pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
        if outcome == ServiceOutcome::Repeated {
            repeats += 1;
            assert_eq!(
                *sink.samples_per_emit.last().unwrap(),
                1600,
                "a video repeat must still submit a 1600-sample audio chunk (#148)"
            );
        }
    }
    assert!(repeats > 0, "sub-grid content must produce video repeats");
}

#[test]
fn audio_stats_report_the_grid_and_enabled_flag() {
    let (pacer, _clk) = anchored_pacer();
    let stats = pacer.audio_stats();
    assert!(stats.enabled, "a paced pacer reports audio enabled");
    assert_eq!(stats.samples_per_boundary, 1600);
    assert_eq!(stats.underruns, 0);
}

#[test]
fn anchor_empties_the_audio_buffer() {
    // Build up buffered audio (over-rate feed), then a seek / new song: the
    // audio buffer starts empty on the new origin.
    let (mut pacer, clk) = anchored_pacer();
    let next = Cell::new(0i64);
    let mut sink = AudioRecordingSink::default();
    for k in 1..=300i64 {
        clk.set(b(k));
        pacer.service(
            || {
                let j = next.get();
                next.set(j + 1);
                Some(frame_due_with_audio(b(j + 1), 1608))
            },
            &mut sink,
        );
    }
    assert!(
        pacer.audio_stats().buffer_ms > 0,
        "buffer should hold audio"
    );
    clk.set(b(301));
    pacer.anchor();
    assert_eq!(
        pacer.audio_stats().buffer_ms,
        0,
        "anchor empties the buffer"
    );
}

#[test]
fn audio_resume_reset_clears_audio_but_keeps_the_video_pending() {
    // Build up some buffered audio and park a video frame, then Resume: the audio
    // buffer is flushed (item 4) while the VIDEO pending/anchor survive.
    let (mut pacer, clk) = anchored_pacer();
    let next = Cell::new(0i64);
    let mut sink = AudioRecordingSink::default();
    for k in 1..=300i64 {
        clk.set(b(k));
        pacer.service(
            || {
                let j = next.get();
                next.set(j + 1);
                Some(frame_due_with_audio(b(j + 1), 1608))
            },
            &mut sink,
        );
    }
    assert!(
        pacer.audio_stats().buffer_ms > 0,
        "buffer should hold audio"
    );
    let had_pending = pacer.has_pending();

    pacer.audio_resume_reset();
    let stats = pacer.audio_stats();
    assert_eq!(stats.buffer_ms, 0, "Resume empties the audio buffer");
    assert_eq!(
        pacer.has_pending(),
        had_pending,
        "Resume must NOT touch the video pending/anchor"
    );
}

#[test]
fn take_eos_tail_flushes_remaining_audio_zero_filled_to_a_full_boundary() {
    // Leave a residual in the audio buffer (over-rate feed), then flush the tail:
    // one final chunk of exactly samples_per_boundary, then nothing (item 4).
    let (mut pacer, clk) = anchored_pacer();
    let next = Cell::new(0i64);
    let mut sink = AudioRecordingSink::default();
    for k in 1..=400i64 {
        clk.set(b(k));
        pacer.service(
            || {
                let j = next.get();
                next.set(j + 1);
                Some(frame_due_with_audio(b(j + 1), 1608))
            },
            &mut sink,
        );
    }
    let tail = pacer.take_eos_tail();
    assert_eq!(tail.len(), 1, "one final tail chunk");
    let spc = tail[0].data.len() / tail[0].channels as usize;
    assert_eq!(spc, 1600, "tail is zero-filled to a full boundary");

    // Drain to empty; the tail must eventually stop producing chunks.
    let mut guard = 0;
    while !pacer.take_eos_tail().is_empty() {
        guard += 1;
        assert!(guard < 100, "EOS tail must terminate");
    }
    assert!(
        pacer.take_eos_tail().is_empty(),
        "an empty buffer yields no tail chunk"
    );
}

#[test]
fn frame_submitter_sink_sends_one_1600_sample_audio_chunk_before_each_video() {
    use crate::playback::submitter::FrameSubmitter;
    use sp_ndi::{MockNdiBackend, NdiSender};
    use std::sync::Arc;

    let backend = Arc::new(MockNdiBackend::new());
    // Paced sender: clock_video=false (the app owns the cadence).
    let sender = NdiSender::new_with_clocking(backend.clone(), "SP-audio", false, false).unwrap();
    let mut submitter = FrameSubmitter::new(sender, 30, 1);

    let (mut pacer, clk) = anchored_pacer();
    let frames = RefCell::new(VecDeque::new());
    for j in 0..10i64 {
        frames
            .borrow_mut()
            .push_back(frame_due_with_audio(b(j + 1), 1600));
    }
    for k in 1..=10i64 {
        clk.set(b(k));
        pacer.service(|| frames.borrow_mut().pop_front(), &mut submitter);
    }

    let calls = backend.calls();
    // Every video async submit is immediately preceded by exactly one audio
    // submit of 1600 samples-per-channel (the boundary chunk).
    let mut video_count = 0;
    for (i, c) in calls.iter().enumerate() {
        if c.starts_with("send_video_async") {
            video_count += 1;
            let prev = &calls[i - 1];
            assert!(
                prev.contains("send_audio") && prev.contains("spc=1600"),
                "video at {i} must be preceded by a 1600-sample audio submit, prev={prev}"
            );
        }
    }
    assert!(
        video_count >= 9,
        "expected ~10 boundary emits, got {video_count}"
    );
}

// Media-time A/V alignment acceptance (#148 design v2), nested here so
// `pacer.rs` stays under the 1000-line cap.
#[path = "pacer_tests_av_align.rs"]
mod pacer_tests_av_align;
