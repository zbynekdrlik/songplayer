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

/// The longest the picture holds the video's last frame while the song's
/// audio plays on past it (#257). Audio is the master clock: a song ends
/// when its AUDIO has played, so a file whose audio outlasts its picture plays
/// the rest on the held last frame ([`SplitSyncedDecoder::next_held`]).
///
/// Real tails are short. SNV's cached songs pair within ±30 ms; the one
/// longer file in four days of its ledger was a dub with 7.8 s of the Live
/// session's drain silence, which the dub worker no longer writes (#257); a
/// dub's own last sentence trails the original by its placement latency
/// (clamped to at most 6 s). 15 s holds all of that with room to spare, while
/// a broken pair (a truncated video) never freezes the wall on one picture
/// for minutes: the audio past the bound is never read, and the pacer counts
/// that song's end as ONE `underrun_end`, so the ledger names the file.
pub const AUDIO_PAST_VIDEO_MAX_MS: u64 = 15_000;

/// The cadence of the held steps when the video reports no usable frame rate
/// (#257).
const HELD_FALLBACK_FPS: u64 = 30;

/// The video frame rates the held steps trust (#257 review), inclusive:
/// 1000/1 would push 15 000 steps into the 15 s bound, and under 0.6 fps one
/// step would carry more than the grid's 2 s cap of audio.
const HELD_MIN_FPS: u64 = 10;
const HELD_MAX_FPS: u64 = 120;

/// The offset (ms) of held step `k` past its base on the video's own frame
/// cadence `num/den`: `⌊k·1000·den/num⌋`, from the base every time, so a
/// long tail never drifts. A rate outside [`HELD_MIN_FPS`]..=[`HELD_MAX_FPS`]
/// (a zero term included) holds at [`HELD_FALLBACK_FPS`].
fn held_offset_ms(k: u64, (num, den): (u32, u32)) -> u64 {
    let (num, den) = (u64::from(num), u64::from(den));
    let (num, den) = if den == 0 || num < HELD_MIN_FPS * den || num > HELD_MAX_FPS * den {
        (HELD_FALLBACK_FPS, 1)
    } else {
        (num, den)
    };
    k * 1000 * den / num
}

/// The position (ms) a seek to `position_ms` asks the VIDEO reader for
/// (#257 review): at most one frame before the video's `duration_ms`, so a
/// seek past the picture's end (a Pause in the held tail, then ▶) lands on a
/// frame the video has. Media Foundation may refuse a position past its end
/// or land at end-of-stream with no frame to hold. The fast-forward and the
/// audio keep `position_ms`. A video of unknown duration (0) is asked the
/// position itself.
fn video_seek_ms(position_ms: u64, duration_ms: u64, frame_rate: (u32, u32)) -> u64 {
    if duration_ms > 0 {
        position_ms.min(duration_ms.saturating_sub(held_offset_ms(1, frame_rate)))
    } else {
        position_ms
    }
}

/// The picture held past the video's end (#257): whether the video ended,
/// where the held steps start and how many went out. A seek starts it anew.
#[derive(Debug, Default)]
struct HeldTail {
    /// The video reader returned end-of-stream since the last seek; it is
    /// not read again until the next seek.
    video_ended: bool,
    /// The last frame delivered since the last seek: the steps follow it.
    last_frame_ms: Option<u64>,
    /// Where the song plays from since the last seek (0 at the open): the
    /// first step when no frame was delivered since (a seek past the last
    /// frame).
    origin_ms: u64,
    /// Steps handed out since the video ended.
    steps: u64,
}

impl HeldTail {
    /// The tail of a song that plays from `origin_ms` (a seek's position).
    fn from_origin(origin_ms: u64) -> Self {
        Self {
            origin_ms,
            ..Self::default()
        }
    }

    /// The next step's time (ms), counted, or `None` once it would lie more
    /// than [`AUDIO_PAST_VIDEO_MAX_MS`] past its base. The base is the last
    /// frame delivered, the first step one frame after it; with no frame
    /// delivered since the seek, the seek's own position, the first step on
    /// it.
    fn next_ms(&mut self, frame_rate: (u32, u32)) -> Option<u64> {
        let (base, first) = match self.last_frame_ms {
            Some(ms) => (ms, 1),
            None => (self.origin_ms, 0),
        };
        let at = base + held_offset_ms(first + self.steps, frame_rate);
        if at > base + AUDIO_PAST_VIDEO_MAX_MS {
            return None;
        }
        self.steps += 1;
        Some(at)
    }
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
    /// The paced path, the only one since #221 lane 3, reads a 250 ms cushion
    /// ahead (#148 v4). The G5 read gate bounds the read-ahead to this lead
    /// plus one chunk.
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
    /// The audio reader returned end-of-stream (#257): every audio sample of
    /// the song has been read. Cleared by a seek.
    audio_eos: bool,
    /// The picture held past the video's end (#257, [`next_held`](Self::next_held)).
    held: HeldTail,
    /// The last frame a seek's fast-forward decoded before the video ended
    /// (#257): the picture a seek to or past the last frame holds.
    frame_before_end: Option<DecodedVideoFrame>,
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
    /// each video frame's timestamp. Its caller is the PACED path (250 ms,
    /// #148 v4; #221 lane 3 deleted the SDK-clocked path and its 1540 ms).
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
            audio_eos: false,
            held: HeldTail::default(),
            frame_before_end: None,
        })
    }

    /// The audio read-ahead deadline past each video frame (ms).
    pub fn audio_lead_ms(&self) -> u64 {
        self.audio_lead_ms
    }

    /// Every audio sample of the song has been handed out with a frame
    /// (#257): the audio reader returned end-of-stream and no chunk waits.
    /// The paced producer passes it on with the frame, so the pacer knows that
    /// a block cut short from then on is the song's own end, not an underrun.
    /// A seek clears it. When the video ends first, the audio past the last
    /// frame's deadline goes out with the held steps
    /// ([`next_held`](Self::next_held)); it stays `false` when the bound
    /// [`AUDIO_PAST_VIDEO_MAX_MS`] leaves audio unread.
    pub fn audio_ended(&self) -> bool {
        self.audio_eos && self.pending_audio.is_empty()
    }

    /// Master-clock duration (audio).
    pub fn duration_ms(&self) -> u64 {
        self.duration_ms
    }

    /// The VIDEO reader's own duration (ms), as its container says (#277):
    /// what an early end of the picture is measured against.
    /// [`duration_ms`](Self::duration_ms) is the audio's, the master clock.
    pub fn video_duration_ms(&self) -> u64 {
        self.video.duration_ms()
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

    /// Forward a seek to both readers. Video first (keyframe-aligned, the one
    /// a broken file refuses): a refused seek then moves nothing, so the
    /// pipeline's report that the song plays on from where it was holds for
    /// both streams (#217 review round 1); audio second (sample-accurate).
    ///
    /// The video reader lands on the previous keyframe (`< position_ms`), so
    /// record `position_ms` as a fast-forward target: [`next_synced`](Self::next_synced)
    /// decodes-and-discards the pre-target video frames before pairing, so the
    /// first delivered frame is at `>= position_ms` and the cushion refills like a
    /// fresh Play (#192 round 5). The video reader is asked at most one frame
    /// before its end (#257 review, `video_seek_ms`): a seek past the
    /// picture's end then fast-forwards to the video's last frame and holds
    /// it from `position_ms` on.
    pub fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        let video_ms = video_seek_ms(
            position_ms,
            self.video.duration_ms(),
            self.video.frame_rate(),
        );
        self.video.seek(video_ms)?;
        self.audio.seek(position_ms)?;
        self.pending_audio.clear();
        self.pending_video_target_ms = Some(position_ms);
        self.audio_eos = false;
        // #257: the video plays again from here, and its held tail with it.
        self.held = HeldTail::from_origin(position_ms);
        self.frame_before_end = None;
        Ok(())
    }

    /// Clear buffered audio (used by the pipeline on pause/restart).
    pub fn clear_buffer(&mut self) {
        self.pending_audio.clear();
    }

    /// Return the next video frame together with all audio chunks whose
    /// timestamps are at or before `video_ts + audio_lead_ms`.
    ///
    /// Returns `Ok(None)` when the video stream has ended; from then on the
    /// video reader is not read again until a seek, and the audio past the
    /// last frame goes out with [`next_held`](Self::next_held) (#257).
    pub fn next_synced(
        &mut self,
    ) -> Result<Option<(DecodedVideoFrame, Vec<DecodedAudioFrame>)>, DecoderError> {
        if self.held.video_ended {
            return Ok(None);
        }
        let Some(video) = self.next_target_video_frame()? else {
            self.held.video_ended = true;
            return Ok(None);
        };
        self.held.last_frame_ms = Some(video.timestamp_ms);
        let audio_frames = self.take_audio_until(video.timestamp_ms + self.audio_lead_ms)?;

        debug!(
            video_ts = video.timestamp_ms,
            audio_chunks = audio_frames.len(),
            "SplitSyncedDecoder paired frame"
        );

        Ok(Some((video, audio_frames)))
    }

    /// One step past the video's end (#257): the time (ms) the held picture,
    /// the video's last frame, shows at, and the audio up to that time plus
    /// the lead, exactly as a frame takes its own. The steps follow the
    /// video's frame cadence from its last frame, or from a seek's position
    /// when the seek landed past the last frame. `None` while the video
    /// plays, once every audio sample went out ([`audio_ended`](Self::audio_ended)),
    /// and past [`AUDIO_PAST_VIDEO_MAX_MS`] (the rest is never read).
    pub fn next_held(&mut self) -> Result<Option<(u64, Vec<DecodedAudioFrame>)>, DecoderError> {
        if !self.held.video_ended || self.audio_ended() {
            return Ok(None);
        }
        let Some(at_ms) = self.held.next_ms(self.video.frame_rate()) else {
            return Ok(None);
        };
        let audio = self.take_audio_until(at_ms + self.audio_lead_ms)?;
        Ok(Some((at_ms, audio)))
    }

    /// The last frame a seek's fast-forward decoded before the video ended
    /// (#257). A seek to or past the last frame delivers no frame; this is
    /// the picture its audio plays on. Handed out once.
    pub fn take_frame_before_end(&mut self) -> Option<DecodedVideoFrame> {
        self.frame_before_end.take()
    }

    /// Every audio chunk at or before `deadline`: the chunks already waiting
    /// first, then a read until a chunk lands past it (kept for the next
    /// call) or the reader ends (#257: [`audio_ended`](Self::audio_ended)).
    fn take_audio_until(&mut self, deadline: u64) -> Result<Vec<DecodedAudioFrame>, DecoderError> {
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
            loop {
                let Some(af) = self.audio.next_samples()? else {
                    // #257: the audio ended; `audio_ended` says so.
                    self.audio_eos = true;
                    break;
                };
                if af.timestamp_ms <= deadline {
                    audio_frames.push(af);
                } else {
                    self.pending_audio.push_back(af);
                    break;
                }
            }
        }
        Ok(audio_frames)
    }

    /// Pull the next video frame to DELIVER, honouring a pending post-seek target.
    ///
    /// After a seek the MF reader lands on the previous keyframe, so decode-and-
    /// discard every frame whose `timestamp_ms < target` (bounded by
    /// [`MAX_SEEK_DISCARD_FRAMES`] so a target past end-of-stream can never spin),
    /// then deliver the first frame at `>= target`. A seek that lands exactly on
    /// the target discards nothing (the boundary is inclusive). End-of-stream
    /// during the fast-forward returns `Ok(None)` and keeps the last frame it
    /// decoded for [`take_frame_before_end`](Self::take_frame_before_end)
    /// (#257). With no pending target the plain next frame is returned. The
    /// target is cleared once consumed.
    fn next_target_video_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let Some(target) = self.pending_video_target_ms else {
            return self.video.next_frame();
        };
        // Consume the target now; the frame we return below is the first at or past
        // it (or the bound-exhausted frame, or end-of-stream).
        self.pending_video_target_ms = None;
        let mut skipped = None;
        for _ in 0..MAX_SEEK_DISCARD_FRAMES {
            match self.video.next_frame()? {
                Some(v) if v.timestamp_ms >= target => return Ok(Some(v)),
                // A pre-target frame (a keyframe-landing artefact) — discard it.
                Some(v) => skipped = Some(v),
                // End-of-stream before reaching the target: no frame left, and
                // the last one decoded is what the audio past it plays on (#257).
                None => {
                    self.frame_before_end = skipped;
                    return Ok(None);
                }
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

#[cfg(test)]
#[path = "split_sync_tests_seek.rs"]
mod tests_seek;

#[cfg(test)]
#[path = "split_sync_tests_end.rs"]
mod tests_end;

#[cfg(test)]
#[path = "split_sync_tests_held.rs"]
mod tests_held;
