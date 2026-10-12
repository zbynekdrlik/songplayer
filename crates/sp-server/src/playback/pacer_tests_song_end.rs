//! #257 — a song played to its natural end loses no audio and counts no
//! underrun.
//!
//! On SNV every song that ended on program counted one underrun or more
//! (the #257 ledger's `pipeline:<id>` loss at each natural end): the block
//! the song's audio ends in was zero-filled after its last sample and counted
//! as an underrun, though nothing was missing, and a song whose audio
//! outlasted the one standby block served after its last frame lost the rest.
//!
//! These tests drive the REAL paced decoder ([`open_paced_decoder`]: the
//! `SplitSyncedDecoder` with its 250 ms lead, the G5 read gate and its
//! `audio_ended`) over mock readers, the pure [`Pacer`], and the paced emit
//! loop's order (`pipeline_paced::decode_and_send_paced`, Windows-only): the
//! producer decodes ahead into the queue (each frame with the decoder's
//! `audio_ended`); per boundary `prepare(target, pop)`, the source's end
//! marked once a popped frame says so, `service`, the end check (`drained &&
//! !has_pending`), then `play_out_audio`. Audio sample values encode their
//! media index (`enc`), so the stream is checked sample by sample. The pins
//! come from a scratch model of the decoder pairing, the pacer and the grid.

use std::collections::VecDeque;

use sp_decoder::{
    AudioStream, DecodedAudioFrame, DecodedVideoFrame, DecoderError, MediaStream, PixelFormat,
    VideoStream,
};
use sp_ndi::AudioFrame;

use super::{RATE, Rec, SPB, anchored, b, dec, enc, frame};
use crate::playback::pacer::{PacedFrame, Pacer, open_paced_decoder};

/// One FLAC packet of a cached song's audio (symphonia's 4096-sample blocks).
const PACKET: i64 = 4096;

/// A bound on the emit loop: every song here ends long before it.
const MAX_BOUNDARIES: usize = 10_000;

/// `frames` video frames at `num/den` fps with the decoder's integer-ms pts:
/// frame `k` at `⌊k·1000·den/num⌋`.
struct Video {
    num: u64,
    den: u64,
    next: u64,
    frames: u64,
}

impl MediaStream for Video {
    fn duration_ms(&self) -> u64 {
        self.frames * 1000 * self.den / self.num
    }
    fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
        Ok(())
    }
}

impl VideoStream for Video {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let ts = self.next * 1000 * self.den / self.num;
        self.next += 1;
        Ok(Some(DecodedVideoFrame {
            data: vec![0u8; 12],
            width: 4,
            height: 2,
            stride: 4,
            timestamp_ms: ts,
            pixel_format: PixelFormat::Nv12,
        }))
    }
    fn width(&self) -> u32 {
        4
    }
    fn height(&self) -> u32 {
        2
    }
    fn frame_rate(&self) -> (u32, u32) {
        (self.num as u32, self.den as u32)
    }
}

/// `samples` of contiguous stereo audio from media 0 in `PACKET`-sample
/// packets (the last one partial), each at its integer-ms time; sample `s` is
/// `enc(s)` on ch0 and `−enc(s)` on ch1.
struct Audio {
    samples: i64,
    next: i64,
}

impl MediaStream for Audio {
    fn duration_ms(&self) -> u64 {
        (self.samples * 1000 / RATE) as u64
    }
    fn seek(&mut self, _ms: u64) -> Result<(), DecoderError> {
        Ok(())
    }
}

impl AudioStream for Audio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        if self.next >= self.samples {
            return Ok(None);
        }
        let start = self.next;
        let end = (start + PACKET).min(self.samples);
        self.next = end;
        let mut data = Vec::with_capacity(((end - start) * 2) as usize);
        for s in start..end {
            data.push(enc(s));
            data.push(-enc(s));
        }
        Ok(Some(DecodedAudioFrame {
            data,
            channels: 2,
            sample_rate: 48_000,
            timestamp_ms: (start * 1000 / RATE) as u64,
        }))
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

/// `pipeline_paced::to_paced_frame` with a 0 ms origin: the frame pts and
/// each chunk's 0-based media time, in 100 ns.
fn to_paced(video: DecodedVideoFrame, audio: Vec<DecodedAudioFrame>) -> PacedFrame {
    let chunks = audio
        .into_iter()
        .map(|af| AudioFrame {
            timecode_100ns: Some(af.timestamp_ms as i64 * 10_000),
            data: af.data,
            channels: af.channels,
            sample_rate: af.sample_rate,
        })
        .collect();
    frame(video.timestamp_ms as i64 * 10_000, chunks)
}

/// A song played to its natural end: the pacer, the blocks it put out (the
/// play-out's included) and how many boundaries the song itself took.
struct Played {
    pacer: Pacer,
    rec: Rec,
    song_boundaries: usize,
}

/// Play `frames` frames at `num/den` fps against `samples` of audio to the
/// song's natural end, as the paced pipeline does.
fn play_to_the_end(num: u64, den: u64, frames: u64, samples: i64) -> Played {
    let video = Video {
        num,
        den,
        next: 0,
        frames,
    };
    let audio = Audio { samples, next: 0 };
    let mut decoder =
        open_paced_decoder(Box::new(video), Box::new(audio)).expect("valid mock readers");
    // The producer, ahead of the emit loop (the queue is full on the box).
    let mut queue: VecDeque<(PacedFrame, bool)> = VecDeque::new();
    while let Some((v, a)) = decoder.next_synced().expect("mock readers never fail") {
        queue.push_back((to_paced(v, a), decoder.audio_ended()));
    }
    let (mut pacer, clk) = anchored();
    let mut rec = Rec::default();
    let mut audio_ended = false;
    for song_boundaries in 1..=MAX_BOUNDARIES {
        let target = pacer.next_boundary_100ns();
        pacer.prepare(target, || {
            let (paced, ended) = queue.pop_front()?;
            audio_ended |= ended;
            Some(paced)
        });
        if audio_ended {
            pacer.audio_source_ended();
        }
        clk.set(target);
        pacer.service(|| None, &mut rec);
        if queue.is_empty() && !pacer.has_pending() {
            pacer.play_out_audio(&mut rec, |_, until| clk.set(until));
            return Played {
                pacer,
                rec,
                song_boundaries,
            };
        }
    }
    panic!("the song never ended");
}

/// The song's underruns as `(start, mid, end, total)`.
fn underruns(pacer: &Pacer) -> (u64, u64, u64, u64) {
    let a = pacer.audio_stats();
    (
        a.underruns_start,
        a.underruns_mid,
        a.underruns_end,
        a.underruns,
    )
}

/// Every block is a full boundary on its own grid slot, b(1), b(2), …; the
/// real samples are media `0..samples`, each once and in order, with no
/// silence between them; after the last one only silence. Returns the
/// index of the block holding the last real sample and how many it holds.
fn assert_every_sample_once_then_silence(rec: &Rec, samples: i64) -> (usize, usize) {
    let mut next = 0i64;
    let mut last = (0, 0);
    for (i, (stamp, block)) in rec.blocks.iter().enumerate() {
        assert_eq!(*stamp, b(i as i64 + 1), "block {i} on its boundary");
        assert_eq!(block.len(), SPB as usize, "block {i} is a full boundary");
        let mut real = 0;
        for (j, &v) in block.iter().enumerate() {
            match dec(v) {
                Some(media) => {
                    assert_eq!(media, next, "block {i} sample {j}: media in order");
                    next += 1;
                    real += 1;
                }
                None => assert!(
                    next == 0 || next == samples,
                    "block {i} sample {j}: silence inside the song (after media {next})"
                ),
            }
        }
        if real > 0 {
            last = (i, real);
        }
    }
    assert_eq!(next, samples, "every sample of the song played");
    last
}

#[test]
fn a_23_976_fps_song_of_equal_length_plays_every_sample_with_no_underrun() {
    // 301 frames = 602 602 samples. The audio ends inside the last frame's
    // own boundary: 1002 real samples, then padding. Before #257 that block
    // was an underrun.
    let samples = 602_602;
    let p = play_to_the_end(24_000, 1001, 301, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (376, 1002)
    );
    assert_eq!(p.song_boundaries, 377);
    assert_eq!(p.rec.blocks.len(), 378, "one play-out boundary");
}

#[test]
fn a_25_fps_song_of_equal_length_ends_in_its_play_out_block_with_no_underrun() {
    // 301 frames = 577 920 samples; the last 320 leave on the play-out
    // boundary after the last frame's.
    let samples = 577_920;
    let p = play_to_the_end(25, 1, 301, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (361, 320)
    );
    assert_eq!(p.song_boundaries, 361);
    assert_eq!(p.rec.blocks.len(), 362);
}

#[test]
fn a_60_fps_song_of_equal_length_plays_every_sample_with_no_underrun() {
    let samples = 480_000;
    let p = play_to_the_end(60, 1, 600, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (299, 1600)
    );
    assert_eq!(p.song_boundaries, 301);
    assert_eq!(p.rec.blocks.len(), 302);
}

#[test]
fn audio_45_ms_longer_than_the_picture_plays_out_over_two_boundaries() {
    // 300 frames at 30 fps = 480 000 samples of picture; the audio runs 45 ms
    // (2160 samples) past it. Before #257 one standby block went out after
    // the last frame and the last 560 samples were dropped.
    let samples = 482_160;
    let p = play_to_the_end(30, 1, 300, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (301, 560)
    );
    assert_eq!(p.song_boundaries, 300);
    assert_eq!(p.rec.blocks.len(), 302, "two play-out boundaries");
}

#[test]
fn a_picture_50_ms_longer_than_its_audio_ends_in_silence_with_no_underrun() {
    // The audio ends 2400 samples before the picture: its last 800 samples
    // leave at b(299), then b(300) and the play-out are silence. Before #257
    // both blocks after the audio's last sample counted as underruns.
    let samples = 477_600;
    let p = play_to_the_end(30, 1, 300, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (298, 800)
    );
    assert_eq!(p.song_boundaries, 300);
    assert_eq!(p.rec.blocks.len(), 301);
}

#[test]
fn a_song_cut_before_its_audio_ends_plays_what_was_read_and_counts_one_end_underrun() {
    // The audio runs 1 s past the picture: the decoder reads it only up to
    // the last frame's 250 ms lead (one packet past it: 491 520 samples), so
    // it never reaches the audio's end. That much plays out over 8
    // boundaries; the rest never plays: one end underrun, counted once.
    let p = play_to_the_end(30, 1, 300, 480_000 + 48_000);
    assert_eq!(underruns(&p.pacer), (0, 0, 1, 1));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, 491_520),
        (307, 320)
    );
    assert_eq!(p.song_boundaries, 300);
    assert_eq!(p.rec.blocks.len(), 308);
}
