//! Trait-based A/V sync that drives a [`VideoStream`] and an [`AudioStream`]
//! with audio-as-master-clock. Cross-platform.

use std::collections::VecDeque;

use tracing::debug;

use crate::error::DecoderError;
use crate::stream::{AudioStream, VideoStream};
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

/// Default tolerance for pairing audio chunks to a video frame (ms).
pub const DEFAULT_TOLERANCE_MS: u64 = 40;

/// Maximum duration disagreement between video and audio sidecars before
/// [`SplitSyncedDecoder::new`] warns.
pub const DURATION_MISMATCH_WARN_MS: u64 = 100;

/// Cross-platform split-file A/V sync driver.
///
/// Takes a video and audio reader behind trait objects and pairs each video
/// frame with all the audio chunks whose timestamps fall before (or within
/// [`DEFAULT_TOLERANCE_MS`] of) that frame. Audio is the master clock: the
/// reported duration is the audio stream's duration and every frame is
/// paired against it.
pub struct SplitSyncedDecoder {
    video: Box<dyn VideoStream>,
    audio: Box<dyn AudioStream>,
    pending_audio: VecDeque<DecodedAudioFrame>,
    tolerance_ms: u64,
    duration_ms: u64,
}

impl std::fmt::Debug for SplitSyncedDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitSyncedDecoder")
            .field("tolerance_ms", &self.tolerance_ms)
            .field("duration_ms", &self.duration_ms)
            .field("pending_audio_count", &self.pending_audio.len())
            .finish_non_exhaustive()
    }
}

impl SplitSyncedDecoder {
    /// Build from owned readers. Performs the validation / mismatch check.
    pub fn new(
        video: Box<dyn VideoStream>,
        audio: Box<dyn AudioStream>,
    ) -> Result<Self, DecoderError> {
        Self::with_tolerance(video, audio, DEFAULT_TOLERANCE_MS)
    }

    /// Like [`new`], but accepts a custom pairing tolerance.
    pub fn with_tolerance(
        video: Box<dyn VideoStream>,
        audio: Box<dyn AudioStream>,
        tolerance_ms: u64,
    ) -> Result<Self, DecoderError> {
        if audio.sample_rate() != 48_000 {
            return Err(DecoderError::Mismatch(format!(
                "audio sample rate must be 48000, got {}",
                audio.sample_rate()
            )));
        }
        let ch = audio.channels();
        if !(1..=2).contains(&ch) {
            return Err(DecoderError::Mismatch(format!(
                "audio channels must be 1 or 2, got {ch}"
            )));
        }
        if video.width() == 0 || video.height() == 0 {
            return Err(DecoderError::Mismatch(format!(
                "video dimensions invalid: {}x{}",
                video.width(),
                video.height()
            )));
        }

        let v_dur = video.duration_ms();
        let a_dur = audio.duration_ms();
        if v_dur.abs_diff(a_dur) > DURATION_MISMATCH_WARN_MS {
            tracing::warn!(
                v_dur,
                a_dur,
                "video/audio duration mismatch beyond {DURATION_MISMATCH_WARN_MS}ms tolerance"
            );
        }

        Ok(Self {
            video,
            audio,
            pending_audio: VecDeque::new(),
            tolerance_ms,
            duration_ms: a_dur,
        })
    }

    /// Master-clock duration (audio).
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// Video width in pixels.
    pub fn width(&self) -> u32 {
        self.video.width()
    }

    /// Video height in pixels.
    pub fn height(&self) -> u32 {
        self.video.height()
    }

    /// Video frame rate forwarded from the reader.
    pub fn frame_rate(&self) -> (u32, u32) {
        self.video.frame_rate()
    }

    /// Forward a seek to both readers. Audio first (sample-accurate), video
    /// second (keyframe-aligned).
    pub fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.audio.seek(position_ms)?;
        self.video.seek(position_ms)?;
        self.pending_audio.clear();
        Ok(())
    }

    /// Clear buffered audio (used by the pipeline on pause/restart).
    pub fn clear_buffer(&mut self) {
        self.pending_audio.clear();
    }

    /// Return the next video frame together with all audio chunks whose
    /// timestamps are at or before `video_ts + tolerance`.
    ///
    /// Returns `Ok(None)` when the video stream has ended.
    pub fn next_synced(
        &mut self,
    ) -> Result<Option<(DecodedVideoFrame, Vec<DecodedAudioFrame>)>, DecoderError> {
        let video = match self.video.next_frame()? {
            Some(v) => v,
            None => return Ok(None),
        };

        let deadline = video.timestamp_ms + self.tolerance_ms;
        let mut audio_frames: Vec<DecodedAudioFrame> = Vec::new();

        while let Some(front) = self.pending_audio.front() {
            if front.timestamp_ms <= deadline {
                audio_frames.push(self.pending_audio.pop_front().unwrap());
            } else {
                break;
            }
        }

        while let Some(af) = self.audio.next_samples()? {
            if af.timestamp_ms <= deadline {
                audio_frames.push(af);
            } else {
                self.pending_audio.push_back(af);
                break;
            }
        }

        debug!(
            video_ts = video.timestamp_ms,
            audio_chunks = audio_frames.len(),
            "SplitSyncedDecoder paired frame"
        );

        Ok(Some((video, audio_frames)))
    }
}

// ---------------------------------------------------------------------------
// Tests — cross-platform, use mock readers.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
}
