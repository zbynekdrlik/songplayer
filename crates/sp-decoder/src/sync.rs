//! A/V synchronization wrapper around [`MediaReader`].

use std::collections::VecDeque;
use std::path::Path;

use tracing::debug;

use crate::error::DecoderError;
use crate::reader::MediaReader;
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

/// Default tolerance for considering audio "close enough" to a video frame.
const DEFAULT_TOLERANCE_MS: u64 = 40;

/// Wraps a [`MediaReader`] and pairs each video frame with all audio samples
/// whose timestamps fall before (or within tolerance of) that video frame.
pub struct SyncedDecoder {
    reader: MediaReader,
    pending_audio: VecDeque<DecodedAudioFrame>,
    tolerance_ms: u64,
}

impl SyncedDecoder {
    /// Create a new synced decoder with the default 40 ms tolerance.
    pub fn new(reader: MediaReader) -> Self {
        Self {
            reader,
            pending_audio: VecDeque::new(),
            tolerance_ms: DEFAULT_TOLERANCE_MS,
        }
    }

    /// Open a file and create a synced decoder in one step.
    pub fn open(path: &Path) -> Result<Self, DecoderError> {
        let reader = MediaReader::open(path)?;
        Ok(Self::new(reader))
    }

    /// Duration of the underlying media in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.reader.duration_ms()
    }

    /// Video stream metadata forwarded from the underlying reader.
    pub fn video_info(&self) -> crate::types::VideoStreamInfo {
        self.reader.video_info()
    }

    /// Clear buffered audio (e.g. when restarting playback).
    pub fn clear_buffer(&mut self) {
        self.pending_audio.clear();
    }

    /// Return the next video frame together with all audio chunks whose
    /// timestamps are at or before `video_ts + tolerance`.
    ///
    /// Returns `None` when the video stream has ended.
    pub fn next_synced(
        &mut self,
    ) -> Result<Option<(DecodedVideoFrame, Vec<DecodedAudioFrame>)>, DecoderError> {
        // 1. Get next video frame.
        let video = match self.reader.next_video_frame()? {
            Some(v) => v,
            None => return Ok(None),
        };

        let deadline = video.timestamp_ms + self.tolerance_ms;

        // 2. Drain audio up to the deadline.
        //    First, consume anything already buffered.
        let mut audio_frames: Vec<DecodedAudioFrame> = Vec::new();

        while let Some(front) = self.pending_audio.front() {
            if front.timestamp_ms <= deadline {
                audio_frames.push(self.pending_audio.pop_front().unwrap());
            } else {
                break;
            }
        }

        // 3. Read more audio from the reader until we pass the deadline.
        loop {
            match self.reader.next_audio_samples()? {
                Some(af) => {
                    if af.timestamp_ms <= deadline {
                        audio_frames.push(af);
                    } else {
                        // Save for the next call.
                        self.pending_audio.push_back(af);
                        break;
                    }
                }
                None => break, // audio stream ended
            }
        }

        debug!(
            video_ts = video.timestamp_ms,
            audio_chunks = audio_frames.len(),
            "Synced frame"
        );

        Ok(Some((video, audio_frames)))
    }
}
