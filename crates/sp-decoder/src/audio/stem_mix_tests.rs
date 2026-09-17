//! Cross-platform unit tests for [`StemMixReader`] (#186). The mixing + ramp
//! algorithm is pure f32 arithmetic — fully exercised on Linux CI with mock
//! streams.

use super::*;
use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;

/// Mock audio stream yielding pre-scripted interleaved f32 packets.
struct MockAudio {
    packets: VecDeque<Vec<f32>>,
    sample_rate: u32,
    channels: u16,
    duration_ms: u64,
    seek_calls: Arc<AtomicU64>,
    last_seek_ms: Arc<AtomicU64>,
}

impl MockAudio {
    fn new(packets: Vec<Vec<f32>>, sample_rate: u32, channels: u16, duration_ms: u64) -> Self {
        Self {
            packets: packets.into(),
            sample_rate,
            channels,
            duration_ms,
            seek_calls: Arc::new(AtomicU64::new(0)),
            last_seek_ms: Arc::new(AtomicU64::new(0)),
        }
    }
    fn seek_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.seek_calls)
    }
    fn last_seek(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.last_seek_ms)
    }
}

impl MediaStream for MockAudio {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.seek_calls.fetch_add(1, Ordering::SeqCst);
        self.last_seek_ms.store(position_ms, Ordering::SeqCst);
        Ok(())
    }
}

impl AudioStream for MockAudio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        Ok(self.packets.pop_front().map(|data| DecodedAudioFrame {
            data,
            channels: self.channels as u32,
            sample_rate: self.sample_rate,
            timestamp_ms: 0, // ignored by StemMixReader
        }))
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
}

fn mock(packets: Vec<Vec<f32>>, sr: u32, ch: u16, dur: u64) -> Box<dyn AudioStream> {
    Box::new(MockAudio::new(packets, sr, ch, dur))
}

/// Drain the reader into one flat interleaved Vec (mixing all packets).
fn drain(reader: &mut StemMixReader) -> Vec<f32> {
    let mut out = Vec::new();
    while let Some(f) = reader.next_samples().unwrap() {
        out.extend(f.data);
    }
    out
}

fn approx(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len(), "length mismatch: {a:?} vs {b:?}");
    for (x, y) in a.iter().zip(b.iter()) {
        assert!((x - y).abs() < 1e-6, "value mismatch: {a:?} vs {b:?}");
    }
}

/// Three stereo mock streams at 48 kHz with the given fixed gains (stable ⇒ no
/// ramp: `current` initialises to the target at construction).
fn reader3(
    a: Vec<Vec<f32>>,
    b: Vec<Vec<f32>>,
    c: Vec<Vec<f32>>,
    ga: f32,
    gb: f32,
    gc: f32,
) -> StemMixReader {
    StemMixReader::new(
        vec![
            mock(a, 48_000, 2, 1000),
            mock(b, 48_000, 2, 1000),
            mock(c, 48_000, 2, 1000),
        ],
        vec![shared_gain(ga), shared_gain(gb), shared_gain(gc)],
    )
    .unwrap()
}

// ── mixing math ───────────────────────────────────────────────────────────

#[test]
fn n_stream_sum_with_unit_gains() {
    let mut r = reader3(
        vec![vec![0.2, 0.2]],
        vec![vec![0.1, 0.1]],
        vec![vec![0.05, 0.05]],
        1.0,
        1.0,
        1.0,
    );
    approx(&drain(&mut r), &[0.35, 0.35]);
}

#[test]
fn full_mix_preset_plays_original_only() {
    // original=1, vocals=0, instrumental=0 → original passthrough, no artefacts.
    let mut r = reader3(
        vec![vec![0.3, 0.3]],
        vec![vec![0.9, 0.9]],
        vec![vec![0.7, 0.7]],
        1.0,
        0.0,
        0.0,
    );
    approx(&drain(&mut r), &[0.3, 0.3]);
}

#[test]
fn instrumental_only_preset_plays_instrumental() {
    // original=0, vocals=0, instrumental=1 → true karaoke.
    let mut r = reader3(
        vec![vec![0.3, 0.3]],
        vec![vec![0.9, 0.9]],
        vec![vec![0.7, 0.7]],
        0.0,
        0.0,
        1.0,
    );
    approx(&drain(&mut r), &[0.7, 0.7]);
}

#[test]
fn clamps_positive_and_negative_overshoot() {
    let mut r = reader3(
        vec![vec![0.9, -0.9]],
        vec![vec![0.9, -0.9]],
        vec![vec![0.9, -0.9]],
        1.0,
        1.0,
        1.0,
    );
    // 2.7 → 1.0 ; -2.7 → -1.0
    approx(&drain(&mut r), &[1.0, -1.0]);
}

#[test]
fn single_stream_passthrough_at_unit_gain() {
    let mut r = StemMixReader::new(
        vec![mock(vec![vec![0.25, -0.25, 0.5, -0.5]], 48_000, 2, 1000)],
        vec![shared_gain(1.0)],
    )
    .unwrap();
    approx(&drain(&mut r), &[0.25, -0.25, 0.5, -0.5]);
}

#[test]
fn mixes_across_mismatched_packet_boundaries() {
    // stream0: two 1-frame packets; stream1: one 2-frame packet; stream2 silent.
    let mut r = reader3(
        vec![vec![0.1, 0.1], vec![0.2, 0.2]],
        vec![vec![0.05, 0.05, 0.05, 0.05]],
        vec![vec![0.0, 0.0, 0.0, 0.0]],
        1.0,
        1.0,
        1.0,
    );
    approx(&drain(&mut r), &[0.15, 0.15, 0.25, 0.25]);
}

// ── ramps (the #186 fix) ────────────────────────────────────────────────────

#[test]
fn ramp_reaches_target_in_exactly_ramp_samples_no_overstep() {
    // sr=200 → ramp_samples = 200/20 = 10 frames, step = 0.1. Mono ⇒ 1 frame =
    // 1 sample. A constant-1.0 stream makes out[i] == the applied gain at frame i.
    let target = shared_gain(0.0);
    let stream = mock(vec![vec![1.0; 40]], 200, 1, 1000);
    let mut r = StemMixReader::new(vec![stream], vec![Arc::clone(&target)]).unwrap();
    assert_eq!(r.ramp_samples(), 10, "ramp_samples must be sample_rate/20");

    // Operator flips the preset: raise this stream's gain to 1.0.
    target.store(gain_to_bits(1.0), Ordering::Relaxed);
    let out = drain(&mut r);

    // Reached the target at EXACTLY ramp_samples frames (index 9), not before.
    assert!(out[8] < 1.0 - 1e-6, "not reached before ramp_samples: out[8]={}", out[8]);
    assert!((out[9] - 1.0).abs() < 1e-6, "reached at ramp_samples: out[9]={}", out[9]);
    // Monotonic, and no per-sample step exceeds 1/ramp_samples.
    let step = 1.0 / 10.0;
    for w in out.windows(2) {
        assert!(w[1] - w[0] <= step + 1e-6, "max per-sample step ≤ 1/ramp_samples: {w:?}");
        assert!(w[1] >= w[0] - 1e-6, "monotonic toward target: {w:?}");
    }
}

#[test]
fn preset_change_down_crossfades_no_click() {
    // Same rig, dropping 1.0 → 0.0: a smooth 50 ms fade-down, no discontinuity.
    let target = shared_gain(1.0);
    let stream = mock(vec![vec![1.0; 40]], 200, 1, 1000);
    let mut r = StemMixReader::new(vec![stream], vec![Arc::clone(&target)]).unwrap();
    target.store(gain_to_bits(0.0), Ordering::Relaxed);
    let out = drain(&mut r);
    assert!((out[9] - 0.0).abs() < 1e-6, "reached 0 at ramp_samples: out[9]={}", out[9]);
    for w in out.windows(2) {
        assert!(w[0] - w[1] <= 0.1 + 1e-6, "no downward step > 1/ramp_samples: {w:?}");
    }
}

#[test]
fn song_opens_at_preset_no_fade_in() {
    // current initialises to the target at construction, so the FIRST sample is
    // already at full gain — no unwanted 50 ms fade-in at song start.
    let mut r = StemMixReader::new(
        vec![mock(vec![vec![1.0, 1.0]], 48_000, 2, 1000)],
        vec![shared_gain(1.0)],
    )
    .unwrap();
    approx(&drain(&mut r), &[1.0, 1.0]);
}

// ── EOS / silence / seek ────────────────────────────────────────────────────

#[test]
fn returns_none_when_all_streams_end() {
    let mut r = reader3(
        vec![vec![0.1, 0.1]],
        vec![vec![0.2, 0.2]],
        vec![vec![0.3, 0.3]],
        1.0,
        1.0,
        1.0,
    );
    assert!(r.next_samples().unwrap().is_some());
    assert!(r.next_samples().unwrap().is_none());
    assert!(r.next_samples().unwrap().is_none());
}

#[test]
fn drains_longest_stream_against_silence_when_others_end() {
    // stream0 has 2 frames, the others only 1 — the extra frame still plays
    // (others mixed as silence) so the song reaches its end.
    let mut r = reader3(
        vec![vec![0.2, 0.2, 0.4, 0.4]],
        vec![vec![0.1, 0.1]],
        vec![vec![0.05, 0.05]],
        1.0,
        1.0,
        1.0,
    );
    // frame0: 0.2+0.1+0.05 ; frame1: 0.4 + silence + silence
    approx(&drain(&mut r), &[0.35, 0.35, 0.4, 0.4]);
}

#[test]
fn timestamps_are_monotonic_from_cumulative_frames() {
    // sr=1000, stereo ⇒ 1 frame = 1 ms. Two 2-frame packets per stream.
    let mut r = StemMixReader::new(
        vec![
            mock(vec![vec![0.0; 4], vec![0.0; 4]], 1000, 2, 100),
            mock(vec![vec![0.0; 4], vec![0.0; 4]], 1000, 2, 100),
        ],
        vec![shared_gain(1.0), shared_gain(1.0)],
    )
    .unwrap();
    let a = r.next_samples().unwrap().unwrap();
    let b = r.next_samples().unwrap().unwrap();
    assert_eq!(a.timestamp_ms, 0);
    assert_eq!(b.timestamp_ms, 2); // 2 frames at 1000 Hz = 2 ms
    assert_eq!(a.sample_rate, 1000);
    assert_eq!(a.channels, 2);
}

#[test]
fn seek_forwards_to_all_streams_clears_buffers_and_reanchors_ts() {
    let s0 = MockAudio::new(vec![vec![0.1, 0.1], vec![0.2, 0.2]], 1000, 2, 10_000);
    let s1 = MockAudio::new(vec![vec![0.0, 0.0], vec![0.0, 0.0]], 1000, 2, 10_000);
    let s2 = MockAudio::new(vec![vec![0.0, 0.0], vec![0.0, 0.0]], 1000, 2, 10_000);
    let c0 = s0.seek_counter();
    let c1 = s1.seek_counter();
    let c2 = s2.seek_counter();
    let p0 = s0.last_seek();
    let mut r = StemMixReader::new(
        vec![Box::new(s0), Box::new(s1), Box::new(s2)],
        vec![shared_gain(1.0), shared_gain(1.0), shared_gain(1.0)],
    )
    .unwrap();

    let _ = r.next_samples().unwrap();
    r.seek(5000).unwrap();
    assert_eq!(c0.load(Ordering::SeqCst), 1, "stream0 seek forwarded");
    assert_eq!(c1.load(Ordering::SeqCst), 1, "stream1 seek forwarded");
    assert_eq!(c2.load(Ordering::SeqCst), 1, "stream2 seek forwarded");
    assert_eq!(p0.load(Ordering::SeqCst), 5000, "seek position forwarded");

    let after = r.next_samples().unwrap().unwrap();
    assert_eq!(after.timestamp_ms, 5000, "timestamp re-anchored to the seek");
}

// ── construction guards ─────────────────────────────────────────────────────

#[test]
fn duration_is_the_longest_stream() {
    let r = StemMixReader::new(
        vec![
            mock(vec![], 48_000, 2, 2500),
            mock(vec![], 48_000, 2, 2400),
            mock(vec![], 48_000, 2, 2500),
        ],
        vec![shared_gain(1.0), shared_gain(1.0), shared_gain(1.0)],
    )
    .unwrap();
    assert_eq!(r.duration_ms(), 2500);
    assert_eq!(r.sample_rate(), 48_000);
    assert_eq!(r.channels(), 2);
}

#[test]
fn rejects_sample_rate_mismatch() {
    let err = StemMixReader::new(
        vec![mock(vec![], 48_000, 2, 1000), mock(vec![], 44_100, 2, 1000)],
        vec![shared_gain(1.0), shared_gain(1.0)],
    )
    .unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn rejects_channel_mismatch() {
    let err = StemMixReader::new(
        vec![mock(vec![], 48_000, 2, 1000), mock(vec![], 48_000, 1, 1000)],
        vec![shared_gain(1.0), shared_gain(1.0)],
    )
    .unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn rejects_stream_gain_count_mismatch() {
    let err = StemMixReader::new(
        vec![mock(vec![], 48_000, 2, 1000)],
        vec![shared_gain(1.0), shared_gain(1.0)],
    )
    .unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn rejects_empty_stream_set() {
    let err = StemMixReader::new(vec![], vec![]).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn gain_bits_round_trip() {
    for g in [0.0_f32, 0.3, 0.5, 1.0] {
        assert_eq!(gain_from_bits(gain_to_bits(g)), g);
    }
}
