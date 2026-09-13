//! Pacer audio clock-discipline wiring tests (#148).
//!
//! Drive the pure [`Pacer`] with a settable wall clock and assert the NEW audio
//! contract: audio is decoupled from the video frame decision — the pacer pushes
//! every consumed frame's audio into its `AudioGridBuffer` and, at each active
//! boundary, submits EXACTLY `samples_per_boundary` (1600 @ 48 kHz/30) samples,
//! including on a sub-grid video repeat. `anchor()` clears the buffer + resets
//! the PLL, and a sustained buffer drift drives `applied_ppm` in the correcting
//! direction. `super::*` resolves to the `pacer` module under test.

use super::*;
use crate::playback::wallclock::{SettableClock, WallClock};
use sp_ndi::AudioFrame;
use std::cell::RefCell;
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
        video: vec![0u8; 12],
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
    assert_eq!(stats.applied_ppm, 0.0);
}

#[test]
fn anchor_clears_the_buffer_and_resets_the_pll() {
    let (mut pacer, clk) = anchored_pacer();
    // Over-feed so the buffer grows and the PLL eventually corrects.
    let frames = RefCell::new(VecDeque::new());
    for j in 0..700i64 {
        frames
            .borrow_mut()
            .push_back(frame_due_with_audio(b(j + 1), 1650));
    }
    let mut sink = AudioRecordingSink::default();
    for k in 1..=600i64 {
        clk.set(b(k));
        pacer.service(|| frames.borrow_mut().pop_front(), &mut sink);
    }
    // A sustained growing buffer (file clock fast) must engage a POSITIVE
    // applied_ppm (drain faster) — the correcting direction.
    assert!(
        pacer.audio_stats().applied_ppm > 0.0,
        "sustained growth must drive applied_ppm positive, got {}",
        pacer.audio_stats().applied_ppm
    );

    // anchor() must clear the buffer and reset the PLL back to 0.
    clk.set(b(601));
    pacer.anchor();
    let stats = pacer.audio_stats();
    assert_eq!(stats.applied_ppm, 0.0, "anchor resets the PLL");
    assert_eq!(stats.residual_ppm, 0.0, "anchor clears the residual");
    assert_eq!(stats.buffer_ms, 0, "anchor empties the buffer");
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
