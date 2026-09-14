//! Cross-platform unit tests for [`KaraokeAudioReader`] using mock readers.
//! The mixing algorithm is pure f32 arithmetic — fully exercised on Linux CI.

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
            timestamp_ms: 0, // ignored by KaraokeAudioReader
        }))
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
}

/// Drain the reader into one flat interleaved Vec (mixing all packets).
fn drain(reader: &mut KaraokeAudioReader) -> Vec<f32> {
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

fn reader(
    v: Vec<Vec<f32>>,
    i: Vec<Vec<f32>>,
    vg: f32,
    ig: f32,
) -> KaraokeAudioReader {
    let voc = Box::new(MockAudio::new(v, 48_000, 2, 1000));
    let inst = Box::new(MockAudio::new(i, 48_000, 2, 1000));
    KaraokeAudioReader::new(voc, inst, shared_gain(vg), shared_gain(ig)).unwrap()
}

#[test]
fn mixes_both_stems_with_unit_gains() {
    // FullMix-equivalent: vg=1, ig=1 → sum of the two stems.
    let mut r = reader(
        vec![vec![0.2, 0.2, 0.4, 0.4]],
        vec![vec![0.1, 0.1, 0.1, 0.1]],
        1.0,
        1.0,
    );
    approx(&drain(&mut r), &[0.3, 0.3, 0.5, 0.5]);
}

#[test]
fn vocals_only_zeroes_instrumental() {
    let mut r = reader(
        vec![vec![0.2, 0.2, 0.4, 0.4]],
        vec![vec![0.1, 0.1, 0.1, 0.1]],
        1.0,
        0.0,
    );
    approx(&drain(&mut r), &[0.2, 0.2, 0.4, 0.4]);
}

#[test]
fn instrumental_only_zeroes_vocals() {
    // True karaoke: vg=0, ig=1 → the instrumental stem alone.
    let mut r = reader(
        vec![vec![0.2, 0.2, 0.4, 0.4]],
        vec![vec![0.1, 0.1, 0.1, 0.1]],
        0.0,
        1.0,
    );
    approx(&drain(&mut r), &[0.1, 0.1, 0.1, 0.1]);
}

#[test]
fn karaoke_low_attenuates_vocals_keeps_instrumental() {
    let mut r = reader(
        vec![vec![0.2, 0.2, 0.4, 0.4]],
        vec![vec![0.1, 0.1, 0.1, 0.1]],
        0.3,
        1.0,
    );
    // 0.2*0.3 + 0.1 = 0.16 ; 0.4*0.3 + 0.1 = 0.22
    approx(&drain(&mut r), &[0.16, 0.16, 0.22, 0.22]);
}

#[test]
fn clamps_positive_and_negative_overshoot() {
    let mut r = reader(
        vec![vec![0.9, -0.9]],
        vec![vec![0.9, -0.9]],
        1.0,
        1.0,
    );
    // 1.8 → 1.0 ; -1.8 → -1.0
    approx(&drain(&mut r), &[1.0, -1.0]);
}

#[test]
fn mixes_across_mismatched_packet_boundaries() {
    // Vocals: two 1-frame packets; instrumental: one 2-frame packet.
    let mut r = reader(
        vec![vec![0.1, 0.1], vec![0.2, 0.2]],
        vec![vec![0.05, 0.05, 0.05, 0.05]],
        1.0,
        1.0,
    );
    approx(&drain(&mut r), &[0.15, 0.15, 0.25, 0.25]);
}

#[test]
fn live_gain_change_between_chunks_takes_effect() {
    let vg = shared_gain(1.0);
    let voc = Box::new(MockAudio::new(vec![vec![0.2, 0.2], vec![0.2, 0.2]], 48_000, 2, 1000));
    let inst = Box::new(MockAudio::new(vec![vec![0.0, 0.0], vec![0.0, 0.0]], 48_000, 2, 1000));
    let mut r = KaraokeAudioReader::new(voc, inst, Arc::clone(&vg), shared_gain(1.0)).unwrap();

    let first = r.next_samples().unwrap().unwrap();
    approx(&first.data, &[0.2, 0.2]);
    // Operator drops the vocal slider mid-song.
    vg.store(gain_to_bits(0.5), Ordering::Relaxed);
    let second = r.next_samples().unwrap().unwrap();
    approx(&second.data, &[0.1, 0.1]);
}

#[test]
fn timestamps_are_monotonic_from_cumulative_frames() {
    // sr=1000, stereo ⇒ 1 frame = 1 ms. Two 2-frame packets.
    let voc = Box::new(MockAudio::new(vec![vec![0.0; 4], vec![0.0; 4]], 1000, 2, 100));
    let inst = Box::new(MockAudio::new(vec![vec![0.0; 4], vec![0.0; 4]], 1000, 2, 100));
    let mut r = KaraokeAudioReader::new(voc, inst, shared_gain(1.0), shared_gain(1.0)).unwrap();
    let a = r.next_samples().unwrap().unwrap();
    let b = r.next_samples().unwrap().unwrap();
    assert_eq!(a.timestamp_ms, 0);
    assert_eq!(b.timestamp_ms, 2); // 2 frames at 1000 Hz = 2 ms
    assert_eq!(a.sample_rate, 1000);
    assert_eq!(a.channels, 2);
}

#[test]
fn returns_none_when_both_stems_end() {
    let mut r = reader(vec![vec![0.1, 0.1]], vec![vec![0.2, 0.2]], 1.0, 1.0);
    assert!(r.next_samples().unwrap().is_some());
    assert!(r.next_samples().unwrap().is_none());
    assert!(r.next_samples().unwrap().is_none());
}

#[test]
fn drains_longer_stem_against_silence_when_other_ends() {
    // Vocals has 2 frames, instrumental only 1 — the extra vocal frame still
    // plays (mixed against instrumental silence) so the song reaches its end.
    let mut r = reader(
        vec![vec![0.2, 0.2, 0.4, 0.4]],
        vec![vec![0.1, 0.1]],
        1.0,
        1.0,
    );
    // frame0: 0.2+0.1 ; frame1: 0.4 + silence
    approx(&drain(&mut r), &[0.3, 0.3, 0.4, 0.4]);
}

#[test]
fn duration_is_vocals_master_duration() {
    let voc = Box::new(MockAudio::new(vec![], 48_000, 2, 2500));
    let inst = Box::new(MockAudio::new(vec![], 48_000, 2, 2500));
    let r = KaraokeAudioReader::new(voc, inst, shared_gain(1.0), shared_gain(1.0)).unwrap();
    assert_eq!(r.duration_ms(), 2500);
    assert_eq!(r.sample_rate(), 48_000);
    assert_eq!(r.channels(), 2);
}

#[test]
fn seek_forwards_to_both_clears_buffers_and_reanchors_ts() {
    let voc = MockAudio::new(vec![vec![0.1, 0.1], vec![0.2, 0.2]], 1000, 2, 10_000);
    let inst = MockAudio::new(vec![vec![0.0, 0.0], vec![0.0, 0.0]], 1000, 2, 10_000);
    let vc = voc.seek_counter();
    let ic = inst.seek_counter();
    let vpos = voc.last_seek();
    let mut r = KaraokeAudioReader::new(
        Box::new(voc),
        Box::new(inst),
        shared_gain(1.0),
        shared_gain(1.0),
    )
    .unwrap();

    // Pull one chunk so buffers may hold leftovers, then seek.
    let _ = r.next_samples().unwrap();
    r.seek(5000).unwrap();
    assert_eq!(vc.load(Ordering::SeqCst), 1, "vocals seek forwarded");
    assert_eq!(ic.load(Ordering::SeqCst), 1, "instrumental seek forwarded");
    assert_eq!(vpos.load(Ordering::SeqCst), 5000, "seek position forwarded");

    // Next emitted chunk timestamp is anchored at the seek position (5000 ms at
    // 1000 Hz ⇒ 5000 frames ⇒ 5000 ms), not continuing from before the seek.
    let after = r.next_samples().unwrap().unwrap();
    assert_eq!(after.timestamp_ms, 5000);
}

#[test]
fn rejects_sample_rate_mismatch() {
    let voc = Box::new(MockAudio::new(vec![], 48_000, 2, 1000));
    let inst = Box::new(MockAudio::new(vec![], 44_100, 2, 1000));
    let err = KaraokeAudioReader::new(voc, inst, shared_gain(1.0), shared_gain(1.0)).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn rejects_channel_mismatch() {
    let voc = Box::new(MockAudio::new(vec![], 48_000, 2, 1000));
    let inst = Box::new(MockAudio::new(vec![], 48_000, 1, 1000));
    let err = KaraokeAudioReader::new(voc, inst, shared_gain(1.0), shared_gain(1.0)).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn gain_bits_round_trip() {
    for g in [0.0_f32, 0.3, 0.5, 1.0] {
        assert_eq!(gain_from_bits(gain_to_bits(g)), g);
    }
}
