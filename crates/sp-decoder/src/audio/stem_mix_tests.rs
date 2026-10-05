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

/// Drain the reader into one flat interleaved Vec (mixing all packets). A
/// reader with nothing to emit returns `None`, never an empty chunk: under a
/// mutant that emits empty chunks this fails at once instead of spinning
/// until the mutation run's timeout (#210: the gate covers stem_mix.rs now).
fn drain(reader: &mut StemMixReader) -> Vec<f32> {
    let mut out = Vec::new();
    while let Some(f) = reader.next_samples().unwrap() {
        assert!(
            !f.data.is_empty(),
            "an empty chunk: nothing to emit is None"
        );
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

/// #184: an over comes out at the limiter's ceiling (0.98, −0.18 dBFS), not
/// clamped flat at full scale. The frame [2.7, −2.7] needs the gain 0.98/2.7,
/// both channels take it (one stereo-linked gain), so ±2.7 → ±0.98.
#[test]
fn limits_positive_and_negative_overshoot_to_the_ceiling() {
    let mut r = reader3(
        vec![vec![0.9, -0.9]],
        vec![vec![0.9, -0.9]],
        vec![vec![0.9, -0.9]],
        1.0,
        1.0,
        1.0,
    );
    approx(&drain(&mut r), &[0.98, -0.98]);
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

// ── #184: the peak limiter after the sum (was a hard clamp at ±1.0) ─────────

/// One mono stream at gain 1 and `sr` Hz playing `samples` as one packet.
fn mono(samples: Vec<f32>, sr: u32) -> StemMixReader {
    StemMixReader::new(
        vec![mock(vec![samples], sr, 1, 1000)],
        vec![shared_gain(1.0)],
    )
    .unwrap()
}

/// The release fixture: 1000 Hz mono (the release factor is then exactly
/// 1 − 1/(0.050 s × 1000 Hz) = 0.98 per frame), three frames at 0.5, ONE over
/// at 1.96 (it needs the gain 0.98/1.96 = 0.5), then 1000 frames at 0.5.
fn release_fixture() -> Vec<f32> {
    let mut s = vec![0.5_f32; 3];
    s.push(1.96);
    s.extend(std::iter::repeat_n(0.5_f32, 1000));
    s
}

/// A 1.8× over (three streams of a 100 Hz sine at 0.6, gain 1 each): before
/// #184 the clamp cut 1208 of its 1920 samples flat at ±1.0. Limited, no
/// sample reaches full scale; every sample stays at or under the ceiling
/// (0.98, plus one f32 rounding of the gain).
#[test]
fn an_over_never_reaches_full_scale() {
    let sine: Vec<f32> = (0..960)
        .flat_map(|i| {
            let v = (0.6 * (2.0 * std::f64::consts::PI * 100.0 * i as f64 / 48_000.0).sin()) as f32;
            [v, v]
        })
        .collect();
    let mut r = reader3(
        vec![sine.clone()],
        vec![sine.clone()],
        vec![sine],
        1.0,
        1.0,
        1.0,
    );
    let out = drain(&mut r);
    assert_eq!(out.len(), 1920);
    let at_full_scale = out.iter().filter(|s| s.abs() >= 0.999).count();
    assert_eq!(at_full_scale, 0, "no sample may sit at the clamp (±1.0)");
    let peak = out.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
    assert!(
        peak <= 0.98 + 1e-6,
        "every sample at or under the ceiling, peak {peak}"
    );
}

/// One gain for the whole frame: the left channel's over (1.96 → needs 0.5)
/// scales the quiet right channel by the SAME 0.5 (stereo-linked), so the
/// stereo image does not shift.
#[test]
fn both_channels_take_the_over_frames_gain() {
    let mut r = StemMixReader::new(
        vec![mock(vec![vec![1.96, 0.2]], 48_000, 2, 1000)],
        vec![shared_gain(1.0)],
    )
    .unwrap();
    approx(&drain(&mut r), &[0.98, 0.1]);
}

/// Instant attack, smooth release: the over frame comes out at the ceiling,
/// then the gain recovers EXPONENTIALLY (reduction 0.5 × 0.98^k at frame
/// 3 + k), never jumping back to unity. Before #184 the clamp handed frame 4
/// its full 0.5 again, a gain jump of +0.49 in one frame. The stated bound: the gain
/// rises by at most 1/release_frames = 1/50 per frame (the reduction ≤ 1 times
/// 1 − 0.98), and never falls while the input stays quiet.
#[test]
fn the_gain_recovers_at_the_release_rate_after_an_over() {
    let input = release_fixture();
    let mut r = mono(input.clone(), 1_000);
    let out = drain(&mut r);
    assert_eq!(out.len(), input.len());
    approx(&out[..3], &[0.5, 0.5, 0.5]);
    assert!(
        (out[3] - 0.98).abs() < 1e-6,
        "the over sits at the ceiling: {}",
        out[3]
    );
    for k in 1..=4 {
        let want = 0.5 * (1.0 - 0.5 * 0.98_f64.powi(k as i32));
        let got = out[3 + k] as f64;
        assert!(
            (got - want).abs() < 1e-6,
            "frame {}: want {want}, got {got}",
            3 + k
        );
    }
    let gain: Vec<f32> = out.iter().zip(&input).map(|(o, i)| o / i).collect();
    // Window k holds frames k and k + 1; from window 3 on, the input is quiet.
    for (k, w) in gain.windows(2).enumerate().skip(3) {
        let rise = w[1] - w[0];
        assert!(
            rise <= 0.02 + 1e-6,
            "frame {}: the gain jumped by {rise}",
            k + 1
        );
        assert!(
            rise >= -1e-6,
            "frame {}: the gain fell on a quiet frame by {rise}",
            k + 1
        );
    }
}

/// Once the release has run out the mix is bit-identical again. In f32 the
/// gain 1 − 0.5 × 0.98^k first rounds to exactly 1.0 at frame 827 (scratch
/// model of the limiter); from there every frame is `x · 1.0 = x`, bit for
/// bit. Frame 826 is the last one still (slightly) reduced. Before #184 (no
/// release, the clamp) frame 826 was already untouched.
#[test]
fn the_mix_is_bit_identical_again_once_the_release_ends() {
    let input = release_fixture();
    let mut r = mono(input.clone(), 1_000);
    let out = drain(&mut r);
    assert_eq!(out.len(), input.len());
    assert!(
        out[826] < input[826],
        "frame 826 still carries the tail: {}",
        out[826]
    );
    for (n, (o, i)) in out.iter().zip(&input).enumerate().skip(827) {
        assert_eq!(o.to_bits(), i.to_bits(), "frame {n} must be untouched");
    }
}

/// The sum is untouched, bit for bit, while it stays at or under the ceiling:
/// partial gains on three streams, and a lone stream exactly at ±0.98.
#[test]
fn a_mix_at_or_under_the_ceiling_is_bit_identical() {
    let a = vec![0.31_f32, -0.42, 0.05, 0.6];
    let b = vec![0.2_f32, -0.3, 0.9, 0.1];
    let c = vec![0.1_f32, -0.2, 0.0, 0.07];
    let (ga, gb, gc) = (0.7_f32, 0.4_f32, 1.0_f32);
    let mut r = reader3(
        vec![a.clone()],
        vec![b.clone()],
        vec![c.clone()],
        ga,
        gb,
        gc,
    );
    let out = drain(&mut r);
    assert_eq!(out.len(), a.len());
    for (i, o) in out.iter().enumerate() {
        // The reader's own summation order: 0.0, then each stream × its gain.
        let mut acc = 0.0_f32;
        acc += a[i] * ga;
        acc += b[i] * gb;
        acc += c[i] * gc;
        assert_eq!(o.to_bits(), acc.to_bits(), "sample {i}: {o} vs {acc}");
    }

    let mut lone = StemMixReader::new(
        vec![mock(vec![vec![0.98, -0.98]], 48_000, 2, 1000)],
        vec![shared_gain(1.0)],
    )
    .unwrap();
    let out = drain(&mut lone);
    assert_eq!(out[0].to_bits(), 0.98_f32.to_bits());
    assert_eq!(out[1].to_bits(), (-0.98_f32).to_bits());
}

/// A seek starts unrelated audio, so it drops the release tail: right after
/// the seek the quiet frames are untouched, not ducked by the earlier over.
#[test]
fn a_seek_drops_the_release_tail() {
    let mut r = StemMixReader::new(
        vec![mock(
            vec![vec![1.96], vec![0.5, 0.5, 0.5]],
            1_000,
            1,
            10_000,
        )],
        vec![shared_gain(1.0)],
    )
    .unwrap();
    let over = r.next_samples().unwrap().unwrap();
    approx(&over.data, &[0.98]);
    r.seek(0).unwrap();
    let after = r.next_samples().unwrap().unwrap();
    for s in &after.data {
        assert_eq!(
            s.to_bits(),
            0.5_f32.to_bits(),
            "no tail after a seek: {:?}",
            after.data
        );
    }
}

// ── ramps (the #186 fix) ────────────────────────────────────────────────────

#[test]
fn ramp_reaches_target_in_exactly_ramp_samples_no_overstep() {
    // sr=200 → ramp_samples = 200/20 = 10 frames, step = 0.1. Mono ⇒ 1 frame =
    // 1 sample. A constant-0.5 stream makes out[i] == half the applied gain at
    // frame i (#184: a full-scale 1.0 would be an over the limiter scales).
    let target = shared_gain(0.0);
    let stream = mock(vec![vec![0.5; 40]], 200, 1, 1000);
    let mut r = StemMixReader::new(vec![stream], vec![Arc::clone(&target)]).unwrap();
    assert_eq!(r.ramp_samples(), 10, "ramp_samples must be sample_rate/20");

    // Operator flips the preset: raise this stream's gain to 1.0.
    target.store(gain_to_bits(1.0), Ordering::Relaxed);
    let out: Vec<f32> = drain(&mut r).iter().map(|s| s * 2.0).collect();

    // Reached the target at EXACTLY ramp_samples frames (index 9), not before.
    assert!(
        out[8] < 1.0 - 1e-6,
        "not reached before ramp_samples: out[8]={}",
        out[8]
    );
    assert!(
        (out[9] - 1.0).abs() < 1e-6,
        "reached at ramp_samples: out[9]={}",
        out[9]
    );
    // Monotonic, and no per-sample step exceeds 1/ramp_samples.
    let step = 1.0 / 10.0;
    for w in out.windows(2) {
        assert!(
            w[1] - w[0] <= step + 1e-6,
            "max per-sample step ≤ 1/ramp_samples: {w:?}"
        );
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
    assert!(
        (out[9] - 0.0).abs() < 1e-6,
        "reached 0 at ramp_samples: out[9]={}",
        out[9]
    );
    for w in out.windows(2) {
        assert!(
            w[0] - w[1] <= 0.1 + 1e-6,
            "no downward step > 1/ramp_samples: {w:?}"
        );
    }
}

#[test]
fn ramp_snaps_to_a_partial_target_without_overshoot() {
    // sr=200 → step=0.1. Ramp 0 → 0.25: 0.1, 0.2, then SNAP to 0.25 (the
    // remaining 0.05 ≤ step), never overshooting to 0.3 and never snapping early.
    // Pins the snap comparison + direction (kills the ramp-arithmetic mutants an
    // even-division 0→1 ramp cannot distinguish).
    let target = shared_gain(0.0);
    let stream = mock(vec![vec![1.0; 20]], 200, 1, 1000);
    let mut r = StemMixReader::new(vec![stream], vec![Arc::clone(&target)]).unwrap();
    target.store(gain_to_bits(0.25), Ordering::Relaxed);
    let out = drain(&mut r);
    assert!((out[0] - 0.1).abs() < 1e-6, "frame0 = 0.1, got {}", out[0]);
    assert!((out[1] - 0.2).abs() < 1e-6, "frame1 = 0.2, got {}", out[1]);
    assert!(
        (out[2] - 0.25).abs() < 1e-6,
        "frame2 snaps to 0.25, got {}",
        out[2]
    );
    for (idx, v) in out.iter().enumerate() {
        assert!(
            *v <= 0.25 + 1e-6,
            "never overshoots the target at {idx}: {v}"
        );
    }
    assert!((out[out.len() - 1] - 0.25).abs() < 1e-6, "holds the target");
}

#[test]
fn song_opens_at_preset_no_fade_in() {
    // current initialises to the target at construction, so the FIRST sample is
    // already at full gain — no unwanted 50 ms fade-in at song start. (0.5, not
    // 1.0: since #184 a full-scale sample is an over the limiter scales.)
    let mut r = StemMixReader::new(
        vec![mock(vec![vec![0.5, 0.5]], 48_000, 2, 1000)],
        vec![shared_gain(1.0)],
    )
    .unwrap();
    approx(&drain(&mut r), &[0.5, 0.5]);
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
    assert_eq!(
        after.timestamp_ms, 5000,
        "timestamp re-anchored to the seek"
    );
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

// ── #184 G4 stage level probe ─────────────────────────────────────────────

#[test]
fn probe_measures_the_post_gain_output_so_zero_gains_read_the_floor() {
    // Full-scale content on every stream, every gain 0: the reader EMITS
    // silence, and its probe must say so (the owner-path question of G4).
    let mut r = reader3(
        vec![vec![1.0, -1.0, 1.0, -1.0]],
        vec![vec![1.0, -1.0, 1.0, -1.0]],
        vec![vec![1.0, -1.0, 1.0, -1.0]],
        0.0,
        0.0,
        0.0,
    );
    r.hold_probe_window();
    approx(&drain(&mut r), &[0.0, 0.0, 0.0, 0.0]);
    assert_eq!(
        r.pending_level(),
        (sp_core::audio_level::SILENCE_FLOOR_DBFS, 4)
    );
}

/// The probe measures what the reader EMITS: a full-scale input is an over
/// since #184, so the output sits at the limiter's ceiling (0.98) and the probe
/// reads 20·log10(0.98) ≈ −0.18 dBFS, not 0 dBFS.
#[test]
fn probe_reads_a_full_scale_input_at_the_limiter_ceiling() {
    let mut r = StemMixReader::new(
        vec![mock(
            vec![vec![1.0, -1.0], vec![-1.0, 1.0]],
            48_000,
            2,
            1000,
        )],
        vec![shared_gain(1.0)],
    )
    .unwrap();
    r.hold_probe_window();
    drain(&mut r);
    let (db, n) = r.pending_level();
    let ceiling_db = 20.0 * 0.98_f32.log10();
    assert!(
        (db - ceiling_db).abs() < 1e-4,
        "a full-scale input must read the ceiling {ceiling_db} dBFS, got {db}"
    );
    assert_eq!(n, 4);
}

#[test]
fn gains_id_identifies_the_shared_atomics() {
    let handles = vec![shared_gain(1.0), shared_gain(0.0)];
    let r = StemMixReader::new(
        vec![mock(vec![], 48_000, 2, 1000), mock(vec![], 48_000, 2, 1000)],
        handles.clone(),
    )
    .unwrap();
    // The reader holds the SAME Arcs it was given — its id is the first one's
    // address, identical to what a holder of the handles computes.
    assert_eq!(r.gains_id(), Arc::as_ptr(&handles[0]) as usize);
    assert_eq!(r.gains_id(), gains_id(&handles));
    // A different set of atomics with equal VALUES has a different id.
    let other = vec![shared_gain(1.0), shared_gain(0.0)];
    assert_ne!(gains_id(&other), gains_id(&handles));
    assert_eq!(gains_id(&[]), 0);
}

#[test]
fn label_defaults_to_the_stream_count_and_is_settable() {
    let r = reader3(vec![], vec![], vec![], 1.0, 0.0, 0.0);
    assert_eq!(r.label(), "3-stream");
    let r = r.with_label("dub-4:song_audio");
    assert_eq!(r.label(), "dub-4:song_audio");
}

#[test]
fn format_gains_prints_two_decimals() {
    assert_eq!(
        format_gains(&[0.0, 1.0, 0.5, 0.333]),
        "[0.00,1.00,0.50,0.33]"
    );
    assert_eq!(format_gains(&[]), "[]");
}

// ── #210: the mutation gate covers stem_mix.rs (the `audio/` exclusion now
// names only the Symphonia wrapper); these pin what no other test sees ──────

/// The reader's `Debug` prints its own observable state (the boxed streams
/// are not `Debug`).
#[test]
fn debug_prints_the_mixer_state() {
    let r = reader3(vec![], vec![], vec![], 1.0, 1.0, 1.0);
    let text = format!("{r:?}");
    assert!(text.starts_with("StemMixReader {"), "{text}");
    for field in [
        "streams: 3",
        "sample_rate: 48000",
        "channels: 2",
        "label: \"3-stream\"",
    ] {
        assert!(text.contains(field), "{field} missing from {text}");
    }
}

/// One call emits every whole frame the streams overlap on, no more and no
/// fewer: four stereo frames buffered in each stream come out as ONE
/// 8-sample chunk, and a mono reader emits exactly its three buffered
/// samples, none padded. `drain` concatenates the calls, so it cannot see
/// how the frames were shared out between them.
#[test]
fn one_call_emits_exactly_the_overlapping_whole_frames() {
    let mut r = reader3(
        vec![vec![0.1; 8]],
        vec![vec![0.1; 8]],
        vec![vec![0.1; 8]],
        1.0,
        1.0,
        1.0,
    );
    let first = r.next_samples().unwrap().expect("the buffered frames");
    assert_eq!(first.data.len(), 8, "all four buffered frames in one call");
    assert!(r.next_samples().unwrap().is_none(), "nothing left");

    let mut m = mono(vec![0.25, 0.5, 0.75], 1000);
    let first = m.next_samples().unwrap().expect("the buffered samples");
    assert_eq!(
        first.data,
        vec![0.25, 0.5, 0.75],
        "exactly what was buffered"
    );
}
