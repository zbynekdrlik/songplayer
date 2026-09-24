//! Cross-platform unit tests for [`SplitSyncedDecoder`], driven by mock
//! video/audio readers (moved out of `split_sync.rs` for the 1000-line cap).

use super::*;
use crate::stream::MediaStream;

/// Mock video stream that yields a pre-scripted list of frames.
struct MockVideo {
    frames: VecDeque<DecodedVideoFrame>,
    duration_ms: u64,
    width: u32,
    height: u32,
    seek_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl MockVideo {
    fn new(ms_list: &[u64]) -> Self {
        let frames = ms_list
            .iter()
            .map(|&ms| DecodedVideoFrame {
                data: vec![0u8; 6],
                width: 2,
                height: 2,
                stride: 2,
                timestamp_ms: ms,
                pixel_format: crate::types::PixelFormat::Nv12,
            })
            .collect::<VecDeque<_>>();
        let duration_ms = *ms_list.last().unwrap_or(&0);
        Self {
            frames,
            duration_ms,
            width: 2,
            height: 2,
            seek_calls: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn seek_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        std::sync::Arc::clone(&self.seek_calls)
    }
}

impl MediaStream for MockVideo {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
        self.seek_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

impl VideoStream for MockVideo {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        Ok(self.frames.pop_front())
    }
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn frame_rate(&self) -> (u32, u32) {
        (30, 1)
    }
}

/// Mock audio stream that yields chunks with explicit timestamps.
struct MockAudio {
    chunks: VecDeque<DecodedAudioFrame>,
    duration_ms: u64,
    seek_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl MockAudio {
    fn new(ts_list: &[u64], duration_ms: u64) -> Self {
        let chunks = ts_list
            .iter()
            .map(|&ts| DecodedAudioFrame {
                data: vec![0.0; 4],
                channels: 2,
                sample_rate: 48_000,
                timestamp_ms: ts,
            })
            .collect::<VecDeque<_>>();
        Self {
            chunks,
            duration_ms,
            seek_calls: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn seek_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        std::sync::Arc::clone(&self.seek_calls)
    }
}

impl MediaStream for MockAudio {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
        self.seek_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

impl AudioStream for MockAudio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        Ok(self.chunks.pop_front())
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

#[test]
fn rejects_audio_with_wrong_sample_rate() {
    struct Bad;
    impl MediaStream for Bad {
        fn duration_ms(&self) -> u64 {
            1000
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl AudioStream for Bad {
        fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
            Ok(None)
        }
        fn sample_rate(&self) -> u32 {
            44_100
        }
        fn channels(&self) -> u16 {
            2
        }
    }
    let v = Box::new(MockVideo::new(&[0, 33, 66]));
    let a: Box<dyn AudioStream> = Box::new(Bad);
    let err = SplitSyncedDecoder::new(v, a).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn rejects_zero_video_dimensions() {
    struct ZeroVid;
    impl MediaStream for ZeroVid {
        fn duration_ms(&self) -> u64 {
            1000
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl VideoStream for ZeroVid {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
            Ok(None)
        }
        fn width(&self) -> u32 {
            0
        }
        fn height(&self) -> u32 {
            0
        }
        fn frame_rate(&self) -> (u32, u32) {
            (30, 1)
        }
    }
    let v: Box<dyn VideoStream> = Box::new(ZeroVid);
    let a = Box::new(MockAudio::new(&[], 1000));
    let err = SplitSyncedDecoder::new(v, a).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn duration_is_audio_duration() {
    let v = Box::new(MockVideo::new(&[0, 33]));
    let a = Box::new(MockAudio::new(&[], 2500));
    let dec = SplitSyncedDecoder::new(v, a).unwrap();
    assert_eq!(dec.duration_ms(), 2500);
}

#[test]
fn next_synced_pairs_audio_up_to_tolerance() {
    // Video at 0, 50, 100. Audio at 10, 40, 60, 95, 130.
    let v = Box::new(MockVideo::new(&[0, 50, 100]));
    let a = Box::new(MockAudio::new(&[10, 40, 60, 95, 130], 150));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

    // Frame 0 with tolerance 40 — deadline = 40. Audio <= 40: 10, 40.
    let (f1, a1) = dec.next_synced().unwrap().unwrap();
    assert_eq!(f1.timestamp_ms, 0);
    let ts: Vec<u64> = a1.iter().map(|a| a.timestamp_ms).collect();
    assert_eq!(ts, vec![10, 40]);

    // Frame 50 — deadline = 90. Audio <= 90: 60. (95 stays pending.)
    let (f2, a2) = dec.next_synced().unwrap().unwrap();
    assert_eq!(f2.timestamp_ms, 50);
    let ts: Vec<u64> = a2.iter().map(|a| a.timestamp_ms).collect();
    assert_eq!(ts, vec![60]);

    // Frame 100 — deadline = 140. 95 comes out of pending; 130 matches.
    let (f3, a3) = dec.next_synced().unwrap().unwrap();
    assert_eq!(f3.timestamp_ms, 100);
    let ts: Vec<u64> = a3.iter().map(|a| a.timestamp_ms).collect();
    assert_eq!(ts, vec![95, 130]);

    // No more frames.
    assert!(dec.next_synced().unwrap().is_none());
}

#[test]
fn next_synced_returns_none_when_video_ends() {
    let v = Box::new(MockVideo::new(&[]));
    let a = Box::new(MockAudio::new(&[0, 10, 20], 30));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();
    assert!(dec.next_synced().unwrap().is_none());
}

#[test]
fn seek_clears_pending_and_forwards_to_both() {
    let mock_v = MockVideo::new(&[0, 50]);
    let mock_a = MockAudio::new(&[200, 500], 1000);
    let v_counter = mock_v.seek_counter();
    let a_counter = mock_a.seek_counter();

    let mut dec = SplitSyncedDecoder::new(Box::new(mock_v), Box::new(mock_a)).unwrap();

    // Pull one frame first so pending_audio fills.
    let _ = dec.next_synced().unwrap().unwrap();

    dec.seek(500).unwrap();
    assert!(
        dec.pending_audio.is_empty(),
        "pending_audio must be cleared after seek"
    );
    assert_eq!(
        v_counter.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "seek must forward to video reader"
    );
    assert_eq!(
        a_counter.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "seek must forward to audio reader"
    );
}

// ---------------------------------------------------------------
// Mutation-killing tests — each asserts one narrow property so a
// mutant that breaks that property is caught by a specific test.
// ---------------------------------------------------------------

#[test]
fn rejects_zero_width_only() {
    // Width 0, height non-zero. Kills the `||` -> `&&` mutant in
    // the dimension validation (with `&&`, only-width-zero passes).
    struct W0;
    impl MediaStream for W0 {
        fn duration_ms(&self) -> u64 {
            1000
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl VideoStream for W0 {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
            Ok(None)
        }
        fn width(&self) -> u32 {
            0
        }
        fn height(&self) -> u32 {
            120
        }
        fn frame_rate(&self) -> (u32, u32) {
            (30, 1)
        }
    }
    let v: Box<dyn VideoStream> = Box::new(W0);
    let a = Box::new(MockAudio::new(&[], 1000));
    let err = SplitSyncedDecoder::new(v, a).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn rejects_zero_height_only() {
    // Height 0, width non-zero.
    struct H0;
    impl MediaStream for H0 {
        fn duration_ms(&self) -> u64 {
            1000
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl VideoStream for H0 {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
            Ok(None)
        }
        fn width(&self) -> u32 {
            160
        }
        fn height(&self) -> u32 {
            0
        }
        fn frame_rate(&self) -> (u32, u32) {
            (30, 1)
        }
    }
    let v: Box<dyn VideoStream> = Box::new(H0);
    let a = Box::new(MockAudio::new(&[], 1000));
    let err = SplitSyncedDecoder::new(v, a).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn rejects_zero_channels() {
    // Kills the channels range check: `!(1..=2).contains(&0)` is true.
    struct Ch0;
    impl MediaStream for Ch0 {
        fn duration_ms(&self) -> u64 {
            1000
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl AudioStream for Ch0 {
        fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
            Ok(None)
        }
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u16 {
            0
        }
    }
    let v = Box::new(MockVideo::new(&[0, 33]));
    let a: Box<dyn AudioStream> = Box::new(Ch0);
    let err = SplitSyncedDecoder::new(v, a).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn rejects_three_channels() {
    // Upper bound of the channels range check (5.1 surround not allowed).
    struct Ch3;
    impl MediaStream for Ch3 {
        fn duration_ms(&self) -> u64 {
            1000
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl AudioStream for Ch3 {
        fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
            Ok(None)
        }
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u16 {
            3
        }
    }
    let v = Box::new(MockVideo::new(&[0, 33]));
    let a: Box<dyn AudioStream> = Box::new(Ch3);
    let err = SplitSyncedDecoder::new(v, a).unwrap_err();
    assert!(matches!(err, DecoderError::Mismatch(_)));
}

#[test]
fn accepts_mono_audio() {
    // Lower bound of the channels range (mono = 1 is allowed).
    struct Mono;
    impl MediaStream for Mono {
        fn duration_ms(&self) -> u64 {
            1000
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl AudioStream for Mono {
        fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
            Ok(None)
        }
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u16 {
            1
        }
    }
    let v = Box::new(MockVideo::new(&[0, 33]));
    let a: Box<dyn AudioStream> = Box::new(Mono);
    SplitSyncedDecoder::new(v, a).expect("mono must be accepted");
}

#[test]
fn is_duration_mismatch_exact_boundary_is_not_mismatch() {
    // 100ms difference is the exact boundary — must NOT trigger mismatch.
    assert!(!is_duration_mismatch(1000, 1100));
    assert!(!is_duration_mismatch(1100, 1000));
}

#[test]
fn is_duration_mismatch_one_above_boundary_is_mismatch() {
    // 101ms > 100ms — must trigger mismatch.
    assert!(is_duration_mismatch(1000, 1101));
    assert!(is_duration_mismatch(1101, 1000));
}

#[test]
fn is_duration_mismatch_zero_diff_is_not_mismatch() {
    assert!(!is_duration_mismatch(1000, 1000));
}

#[test]
fn accessors_forward_to_underlying_readers() {
    // Kills mutants that replace width/height/frame_rate/duration_ms
    // with 0/1/default by asserting each accessor returns the exact
    // mock-configured value.
    struct W160H120;
    impl MediaStream for W160H120 {
        fn duration_ms(&self) -> u64 {
            2500
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl VideoStream for W160H120 {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
            Ok(None)
        }
        fn width(&self) -> u32 {
            160
        }
        fn height(&self) -> u32 {
            120
        }
        fn frame_rate(&self) -> (u32, u32) {
            (24_000, 1001)
        }
    }
    let v: Box<dyn VideoStream> = Box::new(W160H120);
    let a = Box::new(MockAudio::new(&[], 2500));
    let dec = SplitSyncedDecoder::new(v, a).unwrap();

    assert_eq!(dec.width(), 160);
    assert_eq!(dec.height(), 120);
    assert_eq!(dec.frame_rate(), (24_000, 1001));
    assert_eq!(dec.duration_ms(), 2500);
}

#[test]
fn clear_buffer_empties_pending_audio() {
    // Fill pending_audio by pulling a frame, then call clear_buffer
    // and verify the queue is empty.
    let v = Box::new(MockVideo::new(&[0]));
    let a = Box::new(MockAudio::new(&[500], 1000));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

    // Pull frame 0: deadline = 40, audio at 500 stays pending.
    let _ = dec.next_synced().unwrap().unwrap();
    assert_eq!(
        dec.pending_audio.len(),
        1,
        "expected 1 pending audio chunk after first frame"
    );

    dec.clear_buffer();
    assert!(dec.pending_audio.is_empty());
}

#[test]
fn with_tolerance_honors_custom_value() {
    // Custom tolerance 200ms lets audio at 150 pair with frame 0.
    let v = Box::new(MockVideo::new(&[0]));
    let a = Box::new(MockAudio::new(&[150], 300));
    let mut dec = SplitSyncedDecoder::with_tolerance(v, a, 200).unwrap();
    let (_f, frames) = dec.next_synced().unwrap().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].timestamp_ms, 150);
}

// ---------------------------------------------------------------
// #192 round 5 — post-seek video fast-forward (Approach 1a)
// ---------------------------------------------------------------

/// A cursor-based video mock whose `seek` lands on the latest KEYFRAME at or
/// before the target (models MF landing on the previous keyframe) and whose
/// `next_frame` yields frames from the cursor forward.
struct SeekMockVideo {
    frames: Vec<u64>,
    keyframes: Vec<u64>,
    cursor: usize,
}

impl SeekMockVideo {
    fn new(frames: Vec<u64>, keyframes: Vec<u64>) -> Self {
        Self {
            frames,
            keyframes,
            cursor: 0,
        }
    }
}

impl MediaStream for SeekMockVideo {
    fn duration_ms(&self) -> u64 {
        *self.frames.last().unwrap_or(&0)
    }
    fn seek(&mut self, pos: u64) -> Result<(), DecoderError> {
        // Latest keyframe at or before `pos` (keyframe-aligned, like MF).
        let kf = self
            .keyframes
            .iter()
            .copied()
            .filter(|&k| k <= pos)
            .max()
            .unwrap_or(0);
        self.cursor = self
            .frames
            .iter()
            .position(|&t| t >= kf)
            .unwrap_or(self.frames.len());
        Ok(())
    }
}

impl VideoStream for SeekMockVideo {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let out = self.frames.get(self.cursor).map(|&ts| DecodedVideoFrame {
            data: vec![0u8; 6],
            width: 2,
            height: 2,
            stride: 2,
            timestamp_ms: ts,
            pixel_format: crate::types::PixelFormat::Nv12,
        });
        if out.is_some() {
            self.cursor += 1;
        }
        Ok(out)
    }
    fn width(&self) -> u32 {
        2
    }
    fn height(&self) -> u32 {
        2
    }
    fn frame_rate(&self) -> (u32, u32) {
        (30, 1)
    }
}

/// A cursor-based audio mock whose `seek` repositions SAMPLE-ACCURATELY to the
/// first chunk at or after the target (models the FLAC reader).
struct SeekMockAudio {
    chunks: Vec<u64>,
    cursor: usize,
    duration_ms: u64,
}

impl SeekMockAudio {
    fn new(chunks: Vec<u64>, duration_ms: u64) -> Self {
        Self {
            chunks,
            cursor: 0,
            duration_ms,
        }
    }
}

impl MediaStream for SeekMockAudio {
    fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn seek(&mut self, pos: u64) -> Result<(), DecoderError> {
        self.cursor = self
            .chunks
            .iter()
            .position(|&t| t >= pos)
            .unwrap_or(self.chunks.len());
        Ok(())
    }
}

impl AudioStream for SeekMockAudio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let out = self.chunks.get(self.cursor).map(|&ts| DecodedAudioFrame {
            data: vec![0.0; 4],
            channels: 2,
            sample_rate: 48_000,
            timestamp_ms: ts,
        });
        if out.is_some() {
            self.cursor += 1;
        }
        Ok(out)
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

#[test]
fn seek_fast_forwards_video_to_the_target_and_pairs_audio_from_the_target() {
    // Video keyframe at 0, frames every 33 ms up to ~5.28 s; audio chunks every
    // 85 ms. A seek to 5000 lands the video on keyframe 0 (as MF does) but the
    // first DELIVERED frame must be >= 5000, and it must pair only audio at
    // >= 5000 up to `frame_ts + tolerance`.
    let vframes: Vec<u64> = (0..=160).map(|k| k * 33).collect(); // 0..5280
    let achunks: Vec<u64> = (0..=70).map(|k| k * 85).collect(); // 0..5950
    let v = Box::new(SeekMockVideo::new(vframes, vec![0]));
    let a = Box::new(SeekMockAudio::new(achunks, 6000));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

    dec.seek(5000).unwrap();
    let (frame, audio) = dec.next_synced().unwrap().unwrap();
    assert!(
        frame.timestamp_ms >= 5000,
        "first delivered frame must be at or past the seek target, got {}",
        frame.timestamp_ms
    );
    // 33 * 152 = 5016 is the first frame >= 5000.
    assert_eq!(frame.timestamp_ms, 5016);
    assert!(
        !audio.is_empty(),
        "the cushion must refill — audio pairs from the target"
    );
    assert!(
        audio.iter().all(|a| a.timestamp_ms >= 5000),
        "no pre-seek audio may pair: {:?}",
        audio.iter().map(|a| a.timestamp_ms).collect::<Vec<_>>()
    );
    let deadline = 5016 + DEFAULT_TOLERANCE_MS;
    assert!(
        audio.iter().all(|a| a.timestamp_ms <= deadline),
        "paired audio must be within tolerance of the delivered frame"
    );
}

#[test]
fn seek_discard_is_bounded_and_then_delivers() {
    // A pathological target beyond every frame: the discard must stop at
    // MAX_SEEK_DISCARD_FRAMES and DELIVER the next frame, never spin. 702 frames
    // at ts == index (all far below the 5000 target) → after 600 discards the
    // 601st frame (ts == 600) is delivered.
    let vframes: Vec<u64> = (0..702).collect();
    let v = Box::new(SeekMockVideo::new(vframes, vec![0]));
    let a = Box::new(SeekMockAudio::new(vec![0, 100, 200], 700));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

    dec.seek(5000).unwrap();
    let (frame, _audio) = dec
        .next_synced()
        .unwrap()
        .expect("the bounded discard must deliver a frame, never spin or None here");
    assert_eq!(
        frame.timestamp_ms, 600,
        "after MAX_SEEK_DISCARD_FRAMES (600) discards, the 601st frame is delivered"
    );
}

#[test]
fn seek_landing_exactly_on_the_target_discards_nothing() {
    // A keyframe AT the target: the video seek lands exactly on it, so the first
    // delivered frame IS the target — nothing is discarded (>= is inclusive).
    let vframes = vec![0, 1000, 2000, 3000, 4000, 5000];
    let v = Box::new(SeekMockVideo::new(vframes, vec![0, 3000]));
    let a = Box::new(SeekMockAudio::new(
        vec![0, 1000, 2000, 3000, 4000, 5000],
        5000,
    ));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

    dec.seek(3000).unwrap();
    let (frame, _audio) = dec.next_synced().unwrap().unwrap();
    assert_eq!(
        frame.timestamp_ms, 3000,
        "a seek landing on a keyframe == target delivers that frame, discarding nothing"
    );
}

#[test]
fn seek_forward_then_backward_both_realign() {
    // A forward seek then a backward seek must EACH deliver a first frame at
    // >= its target and pair audio from >= that target — the backward seek must
    // not replay the frames before it.
    let vframes: Vec<u64> = (0..=10).map(|k| k * 1000).collect(); // 0..10000
    let achunks: Vec<u64> = (0..=20).map(|k| k * 500).collect(); // 0..10000
    let v = Box::new(SeekMockVideo::new(vframes, vec![0]));
    let a = Box::new(SeekMockAudio::new(achunks, 10000));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

    dec.seek(5000).unwrap();
    let (f_fwd, a_fwd) = dec.next_synced().unwrap().unwrap();
    assert_eq!(
        f_fwd.timestamp_ms, 5000,
        "forward seek delivers the target frame"
    );
    assert!(a_fwd.iter().all(|a| a.timestamp_ms >= 5000));

    dec.seek(2000).unwrap();
    let (f_back, a_back) = dec.next_synced().unwrap().unwrap();
    assert_eq!(
        f_back.timestamp_ms, 2000,
        "backward seek realigns to its target, not the last-played position"
    );
    assert!(a_back.iter().all(|a| a.timestamp_ms >= 2000));
}

#[test]
fn seek_past_end_of_stream_returns_none_and_clears_the_target() {
    // A seek target beyond the last frame: the fast-forward hits end-of-stream
    // (within the bound) and returns None — the pending target is cleared so a
    // subsequent pull is not stuck fast-forwarding.
    let v = Box::new(SeekMockVideo::new(vec![0, 33, 66], vec![0]));
    let a = Box::new(SeekMockAudio::new(vec![0, 40], 100));
    let mut dec = SplitSyncedDecoder::new(v, a).unwrap();

    dec.seek(5000).unwrap();
    assert!(
        dec.next_synced().unwrap().is_none(),
        "a seek past end-of-stream delivers None, not a spin"
    );
    // Target already cleared — the exhausted stream still returns None.
    assert!(dec.next_synced().unwrap().is_none());
}

// ---------------------------------------------------------------
// #184 G5 — bounded audio read-ahead. `StemMixReader` applies the live
// fader gains when a chunk is READ, so every chunk read ahead of the video
// is fader latency. With ~48 ms audio chunks and 33 ms (30 fps) / 40 ms
// (25 fps) video frames, `next_synced` must not read a chunk while one is
// already waiting past the deadline.
// ---------------------------------------------------------------

/// Audio chunk length used by the read-ahead tests (the box's ~48 ms blocks).
const G5_CHUNK_MS: u64 = 48;

/// Cursor-based audio mock that records how far the reader has been read:
/// the highest timestamp handed out and the number of chunks read.
struct CountingAudio {
    chunks: Vec<u64>,
    cursor: usize,
    max_ts_read: std::sync::Arc<std::sync::atomic::AtomicU64>,
    reads: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl CountingAudio {
    fn new(chunks: Vec<u64>) -> Self {
        Self {
            chunks,
            cursor: 0,
            max_ts_read: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            reads: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

impl MediaStream for CountingAudio {
    fn duration_ms(&self) -> u64 {
        *self.chunks.last().unwrap_or(&0)
    }
    fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
        Ok(())
    }
}

impl AudioStream for CountingAudio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let Some(&ts) = self.chunks.get(self.cursor) else {
            return Ok(None);
        };
        self.cursor += 1;
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.max_ts_read
            .fetch_max(ts, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(DecodedAudioFrame {
            data: vec![0.0; 4],
            channels: 2,
            sample_rate: 48_000,
            timestamp_ms: ts,
        }))
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

/// One `next_synced` call as observed by the read-ahead tests.
struct G5Step {
    deadline: u64,
    delivered: Vec<u64>,
    pending_len: usize,
    max_ts_read: u64,
}

/// Drive `frames` video frames spaced `frame_ms` apart against 48 ms audio
/// chunks covering TWICE the video length (so the reader never runs dry and
/// any read-ahead is visible), recording every call.
fn run_g5(frame_ms: u64, frames: u64) -> (Vec<G5Step>, Vec<u64>) {
    let vframes: Vec<u64> = (0..frames).map(|k| k * frame_ms).collect();
    let n_chunks = 2 * frames * frame_ms / G5_CHUNK_MS + 2;
    let chunks: Vec<u64> = (0..n_chunks).map(|j| j * G5_CHUNK_MS).collect();
    let audio = CountingAudio::new(chunks.clone());
    let max_ts_read = std::sync::Arc::clone(&audio.max_ts_read);
    let mut dec = SplitSyncedDecoder::new(Box::new(MockVideo::new(&vframes)), Box::new(audio))
        .expect("valid mock readers");

    let mut steps = Vec::new();
    while let Some((frame, audio)) = dec.next_synced().unwrap() {
        steps.push(G5Step {
            deadline: frame.timestamp_ms + DEFAULT_TOLERANCE_MS,
            delivered: audio.iter().map(|a| a.timestamp_ms).collect(),
            pending_len: dec.pending_audio.len(),
            max_ts_read: max_ts_read.load(std::sync::atomic::Ordering::SeqCst),
        });
    }
    assert_eq!(steps.len() as u64, frames, "every video frame is delivered");
    (steps, chunks)
}

/// (a) The reader never runs more than one chunk past the deadline and at
/// most one chunk ever waits in `pending_audio`.
fn assert_read_ahead_bounded(frame_ms: u64) {
    let (steps, _) = run_g5(frame_ms, 300);
    let worst_pending = steps.iter().map(|s| s.pending_len).max().unwrap_or(0);
    let final_pending = steps.last().map_or(0, |s| s.pending_len);
    assert!(
        worst_pending <= 1,
        "{frame_ms} ms frames: pending_audio must hold at most one chunk, \
         grew to {worst_pending} (after 300 frames: {final_pending})"
    );
    for (k, s) in steps.iter().enumerate() {
        assert!(
            s.max_ts_read <= s.deadline + G5_CHUNK_MS,
            "{frame_ms} ms frames, frame {k}: audio read up to {} ms but the \
             deadline is {} ms — read-ahead beyond one {G5_CHUNK_MS} ms chunk",
            s.max_ts_read,
            s.deadline
        );
    }
}

/// (b) Bounding the read-ahead loses nothing: each frame gets exactly the
/// chunks in `(previous deadline, deadline]`, so over the run every chunk up
/// to the last deadline is delivered exactly once, in order.
fn assert_no_chunk_lost(frame_ms: u64) {
    let (steps, chunks) = run_g5(frame_ms, 300);
    let mut prev_deadline: Option<u64> = None;
    let mut all_delivered = Vec::new();
    for (k, s) in steps.iter().enumerate() {
        let expected: Vec<u64> = chunks
            .iter()
            .copied()
            .filter(|&ts| prev_deadline.is_none_or(|p| ts > p) && ts <= s.deadline)
            .collect();
        assert_eq!(
            s.delivered, expected,
            "{frame_ms} ms frames, frame {k} (deadline {}): wrong audio pairing",
            s.deadline
        );
        all_delivered.extend_from_slice(&s.delivered);
        prev_deadline = Some(s.deadline);
    }
    let last_deadline = steps.last().map_or(0, |s| s.deadline);
    let expected_all: Vec<u64> = chunks
        .iter()
        .copied()
        .take_while(|&ts| ts <= last_deadline)
        .collect();
    assert_eq!(
        all_delivered, expected_all,
        "{frame_ms} ms frames: every chunk up to the last deadline exactly once, in order"
    );
}

#[test]
fn next_synced_read_ahead_stays_bounded_at_30fps() {
    assert_read_ahead_bounded(33);
}

#[test]
fn next_synced_read_ahead_stays_bounded_at_25fps() {
    assert_read_ahead_bounded(40);
}

#[test]
fn next_synced_bounded_read_ahead_delivers_every_chunk_once_at_30fps() {
    assert_no_chunk_lost(33);
}

#[test]
fn next_synced_bounded_read_ahead_delivers_every_chunk_once_at_25fps() {
    assert_no_chunk_lost(40);
}

#[test]
fn next_synced_reads_only_when_pending_is_empty() {
    // Frame 0 (deadline 40) reads 0 (paired) and 48 (waits). Frame 3
    // (deadline 139) finds 144 still waiting past the deadline, so it must
    // NOT read — the chunk count stays exactly where frame 2 left it.
    let audio = CountingAudio::new((0..20).map(|j| j * G5_CHUNK_MS).collect());
    let reads = std::sync::Arc::clone(&audio.reads);
    let v = Box::new(MockVideo::new(&[0, 33, 66, 99]));
    let mut dec = SplitSyncedDecoder::new(v, Box::new(audio)).unwrap();
    let count = || reads.load(std::sync::atomic::Ordering::SeqCst);

    let _ = dec.next_synced().unwrap().unwrap(); // deadline 40: reads 0, 48
    assert_eq!(count(), 2);
    let _ = dec.next_synced().unwrap().unwrap(); // deadline 73: pops 48, reads 96
    assert_eq!(count(), 3);
    let _ = dec.next_synced().unwrap().unwrap(); // deadline 106: pops 96, reads 144
    assert_eq!(count(), 4);
    let (_f, a) = dec.next_synced().unwrap().unwrap(); // deadline 139: 144 waits
    assert!(a.is_empty(), "144 > 139 must stay pending");
    assert_eq!(
        count(),
        4,
        "no read while a chunk already waits past the deadline"
    );
    assert_eq!(dec.pending_audio.len(), 1);
}
