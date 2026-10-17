//! What the paced decode producer hands the emit thread (#147, #257): each
//! decoded frame with its audio, and past the video's end the HELD last frame
//! with the audio still to play.
//!
//! A song ends when its AUDIO has played (audio is the master clock). Before
//! #257 the producer stopped at the video's end, and the decoder read the
//! audio only up to the last frame plus its 250 ms lead: a file whose audio
//! runs longer lost the rest (on SNV: a dub's 7.8 s tail, one `underrun_end`
//! at every natural end). Now [`DecodeProducer::next_item`] goes on past the
//! video's end with the decoder's held steps (`SplitSyncedDecoder::next_held`),
//! each one the last frame again (an `Arc` bump, never a pixel copy) at the
//! next frame time with the audio up to it plus the lead. They are ordinary
//! queue items, so the emit loop paces them on the grid (one boundary per
//! slot) and serves its commands between them as it does mid-song: a skip
//! or a stop ends the song at once, a seek goes back into the picture. The
//! steps stop when the audio ended, or at `AUDIO_PAST_VIDEO_MAX_MS` (the rest
//! is cut and the song's end counts one `underrun_end`).
//!
//! #277: a video whose picture stops long before the video reader's own
//! duration has no held tail: the song ends there as a FAILURE
//! ([`DecodeProducer::early_end`], `playback::early_end`), never as its
//! natural end.
//!
//! The Windows producer thread (`pipeline_paced::run_decode_producer`) only
//! opens the readers and pushes what this hands it, so every item is built
//! and tested here, on Linux.

use sp_decoder::{DecodedAudioFrame, DecodedVideoFrame, DecoderError, SplitSyncedDecoder};

use crate::playback::early_end::EarlyEnd;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::PacedFrame;
use crate::playback::picture::{CpuMisses, CpuUse, Picture};
use crate::playback::pipeline::real_seek_ms;
use crate::playback::preview::preview_stream::DecodeTaps;

/// A frame handed from the decode producer to the emit consumer (#147): the
/// paced frame, the ABSOLUTE decoded position (ms) for the pipeline's Position
/// events, and whether the decoder had handed out the song's last audio sample
/// with it (#257, `SplitSyncedDecoder::audio_ended`).
pub type QueuedFrame = (PacedFrame, u64, bool);

/// The picture the held steps repeat (#257): the last frame handed on,
/// shared by `Arc`.
struct Held {
    width: u32,
    height: u32,
    stride: u32,
    video: Picture,
}

impl Held {
    /// The picture of a frame handed on.
    fn of(frame: &PacedFrame) -> Self {
        Self {
            width: frame.width,
            height: frame.height,
            stride: frame.stride,
            video: frame.video.clone(),
        }
    }
}

/// The decode producer's own state: the PTS origin, where it is, the seek
/// generation it pushes under, and the picture past the video's end.
pub struct DecodeProducer {
    /// Where the song really plays from since its start or its last seek
    /// (#217), so a frame's PTS is 0-based.
    pts_offset_ms: u64,
    /// The position of the last item handed out (the origin before one):
    /// where a refused seek leaves the decoder (#217).
    current_ms: u64,
    /// The seek generation the producer pushes under.
    epoch: u64,
    /// The last picture handed on: what the held steps show (#257).
    held: Option<Held>,
    /// #277: the video ended long before its own duration (since the start
    /// or the last seek): the song failed.
    early: Option<EarlyEnd>,
    /// #279 P2: held steps whose GPU picture the preview taps could not
    /// take (their audio still offered).
    preview_misses: CpuMisses,
}

impl DecodeProducer {
    /// A producer for a song that really starts at `start_ms` (#217: 0 when
    /// the start seek failed).
    pub fn new(start_ms: u64) -> Self {
        Self {
            pts_offset_ms: start_ms,
            current_ms: start_ms,
            epoch: 0,
            held: None,
            early: None,
            preview_misses: CpuMisses::default(),
        }
    }

    /// The seek generation the producer pushes under.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// #277: the video ended long before its own duration since the start
    /// or the last seek: the song is a failure, not over.
    pub fn early_end(&self) -> Option<EarlyEnd> {
        self.early
    }

    /// #279 P2: the held steps whose GPU picture the preview taps missed.
    pub fn preview_misses(&self) -> u64 {
        self.preview_misses.count()
    }

    /// The next item for the queue, offered to the preview taps on the way
    /// (`preview.md`): the next decoded frame, or past the video's end a held
    /// step (#257). `Ok(None)` at the song's end: the audio ended, the held
    /// steps reached their bound, or no picture was ever decoded to hold.
    /// #277: also when the video ended early ([`Self::early_end`]), with no
    /// held step: measured at the last picture handed out (or the last frame
    /// a seek's fast-forward decoded, else where the song plays from)
    /// against the video reader's own duration.
    pub fn next_item(
        &mut self,
        decoder: &mut SplitSyncedDecoder,
        taps: &DecodeTaps,
    ) -> Result<Option<QueuedFrame>, DecoderError> {
        if let Some((video, audio)) = decoder.next_synced()? {
            taps.offer_frame(&video, &audio);
            let ms = video.timestamp_ms;
            self.current_ms = ms;
            let frame = to_paced_frame(video, audio, self.pts_offset_ms);
            self.held = Some(Held::of(&frame));
            return Ok(Some((frame, ms, decoder.audio_ended())));
        }
        // A seek to or past the last frame: the last frame its fast-forward
        // decoded is the picture to hold.
        let before_end = decoder.take_frame_before_end();
        let at_ms = before_end
            .as_ref()
            .map_or(self.current_ms, |last| last.timestamp_ms);
        if let Some(early) = EarlyEnd::of(at_ms, decoder.video_duration_ms()) {
            self.early = Some(early);
            return Ok(None);
        }
        if let Some(last) = before_end {
            self.held = Some(Held::of(&to_paced_frame(last, Vec::new(), 0)));
        }
        let Some(held) = &self.held else {
            return Ok(None);
        };
        let Some((ms, audio)) = decoder.next_held()? else {
            return Ok(None);
        };
        // #279 P2: a held GPU picture gives the taps no picture (P3b), only
        // its audio.
        match held
            .video
            .cpu_for(CpuUse::Preview, &mut self.preview_misses)
        {
            Some(nv12) => taps.offer_picture(held.width, held.height, held.stride, nv12, &audio),
            None => taps.offer_audio(&audio),
        }
        self.current_ms = ms;
        let frame = PacedFrame {
            pts_ns: pts_ns(ms, self.pts_offset_ms),
            width: held.width,
            height: held.height,
            stride: held.stride,
            video: held.video.clone(),
            audio: paced_audio(audio, self.pts_offset_ms),
        };
        Ok(Some((frame, ms, decoder.audio_ended())))
    }

    /// Seek the decoder and adopt where the song really plays on from (#217,
    /// `real_seek_ms`: the asked position, or where the producer was when the
    /// decoder refused the seek) as the new PTS origin, and the new epoch.
    /// A seek that moved the decoder drops the held picture (#257 review):
    /// the frame it lands on, or the last one its fast-forward decoded,
    /// replaces it; a refused seek keeps it (nothing moved). Returns that
    /// position, which the pipeline reports (`Seeked`).
    pub fn seek(
        &mut self,
        decoder: &mut SplitSyncedDecoder,
        position_ms: u64,
        epoch: u64,
        playlist_id: i64,
    ) -> u64 {
        let mut moved = false;
        let real = real_seek_ms(
            position_ms,
            self.current_ms,
            |ms| {
                let done = decoder.seek(ms);
                moved = done.is_ok();
                done
            },
            playlist_id,
            "paced producer",
        );
        if moved {
            self.held = None;
        }
        // #277: the video plays again from here.
        self.early = None;
        self.pts_offset_ms = real;
        self.current_ms = real;
        self.epoch = epoch;
        real
    }
}

/// A 0-based PTS in ns: `ms` from the origin `pts_offset_ms` (a position
/// before the origin reads 0).
fn pts_ns(ms: u64, pts_offset_ms: u64) -> i64 {
    (ms.saturating_sub(pts_offset_ms) as i64).saturating_mul(1_000_000)
}

/// The decoded audio chunks as the pacer takes them: each keeps its MEDIA
/// time on the same 0-based origin (in `timecode_100ns`), which the pacer
/// uses to align the audio to the picture (#148 design v2). The boundary
/// block it submits is stamped on its boundary (#224).
fn paced_audio(audio: Vec<DecodedAudioFrame>, pts_offset_ms: u64) -> Vec<sp_ndi::AudioFrame> {
    audio
        .into_iter()
        .map(|af| sp_ndi::AudioFrame {
            timecode_100ns: Some((af.timestamp_ms as i64 - pts_offset_ms as i64) * 10_000),
            data: af.data,
            channels: af.channels,
            sample_rate: af.sample_rate,
        })
        .collect()
}

/// Convert an MF-decoded video frame + its audio chunks into a [`PacedFrame`].
/// `pts_offset_ms` is subtracted so the PTS is measured from playback start
/// (0-based), the origin the pacer maps onto the wall grid.
pub fn to_paced_frame(
    video: DecodedVideoFrame,
    audio: Vec<DecodedAudioFrame>,
    pts_offset_ms: u64,
) -> PacedFrame {
    PacedFrame {
        pts_ns: pts_ns(video.timestamp_ms, pts_offset_ms),
        width: video.width,
        height: video.height,
        stride: video.stride,
        // Wrap the decoded NV12 buffer ONCE on the producer thread (#203 2b);
        // every downstream hop (pacer repeat, handoff, submit holdover, a held
        // step) is an `Arc` bump of this allocation, recycled to the frame pool
        // on last drop.
        video: Picture::Cpu(SharedFrame::new(video.data)),
        audio: paced_audio(audio, pts_offset_ms),
    }
}

#[cfg(test)]
#[path = "paced_producer_tests.rs"]
mod tests;
