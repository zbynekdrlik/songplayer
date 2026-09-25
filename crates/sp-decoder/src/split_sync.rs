//! Trait-based A/V sync that drives a [`VideoStream`] and an [`AudioStream`]
//! with audio-as-master-clock. Cross-platform.

use std::collections::VecDeque;

use tracing::debug;

use crate::error::DecoderError;
use crate::stream::{AudioStream, VideoStream};
use crate::types::{DecodedAudioFrame, DecodedVideoFrame};

/// Default tolerance for pairing audio chunks to a video frame (ms).
pub const DEFAULT_TOLERANCE_MS: u64 = 40;

/// Maximum number of video frames [`SplitSyncedDecoder::next_synced`] will
/// decode-and-discard chasing a post-seek target before it gives up and delivers
/// whatever frame it has. A YouTube AV1 keyframe interval is ~150–300 frames; 600
/// (~20 s at 30 fps) covers the worst real case with headroom while BOUNDING the
/// discard so a target past end-of-stream can never spin the decode loop (#192
/// round 5).
pub const MAX_SEEK_DISCARD_FRAMES: u32 = 600;

/// Maximum duration disagreement between video and audio sidecars before
/// [`SplitSyncedDecoder::new`] warns.
pub const DURATION_MISMATCH_WARN_MS: u64 = 100;

/// Pure predicate extracted so unit tests can exercise the `>`
/// comparison at the 100ms boundary without capturing tracing output.
pub(crate) fn is_duration_mismatch(v_dur: u64, a_dur: u64) -> bool {
    v_dur.abs_diff(a_dur) > DURATION_MISMATCH_WARN_MS
}

/// Cross-platform split-file A/V sync driver.
///
/// Takes a video and audio reader behind trait objects and pairs each video
/// frame with all the audio chunks whose timestamps fall before (or within
/// the audio lead of) that frame — [`DEFAULT_TOLERANCE_MS`] unless built with
/// [`with_audio_lead`](Self::with_audio_lead). Audio is the master clock: the
/// reported duration is the audio stream's duration and every frame is
/// paired against it.
pub struct SplitSyncedDecoder {
    video: Box<dyn VideoStream>,
    audio: Box<dyn AudioStream>,
    pending_audio: VecDeque<DecodedAudioFrame>,
    /// How far past each video frame's timestamp the audio is read and handed
    /// out with it: the `next_synced` deadline is `video_ts + audio_lead_ms`.
    /// The pacing-OFF path pairs at 40 ms (or 1540 ms with the wall-clock
    /// emitter); the paced path reads a 250 ms cushion ahead (#148 v4). The G5
    /// read gate bounds the read-ahead to this lead plus one chunk.
    audio_lead_ms: u64,
    duration_ms: u64,
    /// After a seek, the sample-accurate audio target the video must fast-forward
    /// to. `SplitSyncedDecoder::seek` seeks the MF video reader keyframe-aligned,
    /// so it lands on the PREVIOUS keyframe (`< target`); `next_synced` then
    /// decodes-and-discards video frames below `target` before pairing, so the
    /// first delivered frame is at `>= target` and the cushion refills and A/V
    /// realign exactly like a fresh Play (#192 round 5). Cleared on the first
    /// delivered frame.
    pending_video_target_ms: Option<u64>,
}

impl std::fmt::Debug for SplitSyncedDecoder {
    // Debug output is diagnostic-only — never compared for correctness.
    #[cfg_attr(test, mutants::skip)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitSyncedDecoder")
            .field("audio_lead_ms", &self.audio_lead_ms)
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
        Self::with_audio_lead(video, audio, DEFAULT_TOLERANCE_MS)
    }

    /// Like [`new`], but reads (and hands out) audio up to `audio_lead_ms` past
    /// each video frame's timestamp. Callers:
    ///
    /// - the SDK-clocked path with the wall-clock emitter (1540 ms, #192);
    /// - the PACED path (250 ms, #148 v4).
    ///
    /// The paced pacer aligns the audio to the picture by MEDIA time, so there
    /// the lead only changes how much audio is buffered — never which sample
    /// plays with which frame.
    pub fn with_audio_lead(
        video: Box<dyn VideoStream>,
        audio: Box<dyn AudioStream>,
        audio_lead_ms: u64,
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
        if is_duration_mismatch(v_dur, a_dur) {
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
            audio_lead_ms,
            duration_ms: a_dur,
            pending_video_target_ms: None,
        })
    }

    /// The audio read-ahead deadline past each video frame (ms).
    pub fn audio_lead_ms(&self) -> u64 {
        self.audio_lead_ms
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
    ///
    /// The video reader lands on the previous keyframe (`< position_ms`), so
    /// record `position_ms` as a fast-forward target: [`next_synced`](Self::next_synced)
    /// decodes-and-discards the pre-target video frames before pairing, so the
    /// first delivered frame is at `>= position_ms` and the cushion refills like a
    /// fresh Play (#192 round 5).
    pub fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.audio.seek(position_ms)?;
        self.video.seek(position_ms)?;
        self.pending_audio.clear();
        self.pending_video_target_ms = Some(position_ms);
        Ok(())
    }

    /// Clear buffered audio (used by the pipeline on pause/restart).
    pub fn clear_buffer(&mut self) {
        self.pending_audio.clear();
    }

    /// Return the next video frame together with all audio chunks whose
    /// timestamps are at or before `video_ts + audio_lead_ms`.
    ///
    /// Returns `Ok(None)` when the video stream has ended.
    pub fn next_synced(
        &mut self,
    ) -> Result<Option<(DecodedVideoFrame, Vec<DecodedAudioFrame>)>, DecoderError> {
        let video = match self.next_target_video_frame()? {
            Some(v) => v,
            None => return Ok(None),
        };

        let deadline = video.timestamp_ms + self.audio_lead_ms;
        let mut audio_frames: Vec<DecodedAudioFrame> = Vec::new();

        while let Some(front) = self.pending_audio.front() {
            if front.timestamp_ms <= deadline {
                audio_frames.push(self.pending_audio.pop_front().unwrap());
            } else {
                break;
            }
        }

        // Read only when nothing is waiting: a chunk still pending past the
        // deadline means the audio is already ahead. Reading anyway grew
        // `pending_audio` by one chunk per frame (48 ms chunks vs 33/40 ms
        // frames), and `StemMixReader` applies the fader gains at READ time,
        // so that read-ahead was fader latency (#184 G5). The read-ahead stays
        // bounded at `audio_lead_ms` plus one chunk.
        if self.pending_audio.is_empty() {
            while let Some(af) = self.audio.next_samples()? {
                if af.timestamp_ms <= deadline {
                    audio_frames.push(af);
                } else {
                    self.pending_audio.push_back(af);
                    break;
                }
            }
        }

        debug!(
            video_ts = video.timestamp_ms,
            audio_chunks = audio_frames.len(),
            "SplitSyncedDecoder paired frame"
        );

        Ok(Some((video, audio_frames)))
    }

    /// Pull the next video frame to DELIVER, honouring a pending post-seek target.
    ///
    /// After a seek the MF reader lands on the previous keyframe, so decode-and-
    /// discard every frame whose `timestamp_ms < target` (bounded by
    /// [`MAX_SEEK_DISCARD_FRAMES`] so a target past end-of-stream can never spin),
    /// then deliver the first frame at `>= target`. A seek that lands exactly on
    /// the target discards nothing (the boundary is inclusive). End-of-stream
    /// during the fast-forward returns `Ok(None)`. With no pending target the plain
    /// next frame is returned. The target is cleared once consumed.
    fn next_target_video_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let Some(target) = self.pending_video_target_ms else {
            return self.video.next_frame();
        };
        // Consume the target now; the frame we return below is the first at or past
        // it (or the bound-exhausted frame, or end-of-stream).
        self.pending_video_target_ms = None;
        for _ in 0..MAX_SEEK_DISCARD_FRAMES {
            match self.video.next_frame()? {
                Some(v) if v.timestamp_ms >= target => return Ok(Some(v)),
                // A pre-target frame (a keyframe-landing artefact) — discard it.
                Some(_) => continue,
                // End-of-stream before reaching the target: nothing left to play.
                None => return Ok(None),
            }
        }
        // Bound reached: deliver whatever comes next rather than spinning.
        self.video.next_frame()
    }
}

// ---------------------------------------------------------------------------
// Tests — cross-platform, use mock readers.
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "split_sync_tests.rs"]
mod tests;
