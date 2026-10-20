//! #257 — a song played to its natural end loses no audio and counts no
//! underrun, whether its audio ends with the picture or after it.
//!
//! On SNV every song that ended on program counted one underrun or more
//! (the #257 ledger's `pipeline:<id>` loss at each natural end): the block
//! the song's audio ends in was zero-filled after its last sample and counted
//! as an underrun, though nothing was missing, and a song whose audio
//! outlasted the one standby block served after its last frame lost the rest.
//! Then (step 2): the decoder read the audio only up to the last frame plus
//! its 250 ms lead, so a file whose audio runs longer than that (a dub's
//! 7.8 s tail) was cut and counted one `underrun_end`. Now the producer plays
//! the audio past the picture on the held last frame, up to
//! `AUDIO_PAST_VIDEO_MAX_MS`.
//!
//! These tests drive the REAL paced decoder ([`open_paced_decoder`]: the
//! `SplitSyncedDecoder` with its 250 ms lead, the G5 read gate, its
//! `audio_ended` and its held steps), the REAL producer items
//! ([`DecodeProducer`]) running up to the queue's bound ahead, the pure
//! [`Pacer`], and the paced emit loop's order (`pipeline_paced::
//! decode_and_send_paced`, Windows-only): per boundary the command first
//! (a skip ends the song, a seek re-anchors), then `prepare(target, pop)`,
//! the source's end marked once a popped frame says so, `service`, the end
//! check (`drained && !has_pending`), then `play_out_audio`. Audio sample
//! values encode their media index (`enc`), and each frame's first two bytes
//! its index, so the stream and the pictures are checked boundary by
//! boundary. The pins come from a scratch model of the decoder, the
//! producer, the pacer and the grid.

use std::collections::VecDeque;

use sp_decoder::{
    AudioStream, DecodedAudioFrame, DecodedVideoFrame, DecoderError, MediaStream, PixelFormat,
    SplitSyncedDecoder, VideoStream,
};

use super::{RATE, Rec, SPB, anchored, b, dec, enc};
use crate::playback::paced_producer::{DecodeProducer, QueuedFrame};
use crate::playback::pacer::{Pacer, open_paced_decoder};
use crate::playback::preview::PreviewTap;
use crate::playback::preview::preview_stream::{DecodeTaps, StreamTap};

/// One FLAC packet of a cached song's audio (symphonia's 4096-sample blocks).
const PACKET: i64 = 4096;

/// A bound on the emit loop: every song here ends long before it.
const MAX_BOUNDARIES: usize = 10_000;

/// The producer's look-ahead (`pipeline_paced::DECODE_QUEUE_BOUND`).
const QUEUE_BOUND: usize = 12;

/// `frames` video frames at `num/den` fps with the decoder's integer-ms pts:
/// frame `k` at `⌊k·1000·den/num⌋`, its first two bytes `k`. A seek lands on
/// the last keyframe (every 10th frame) at or before the position, as Media
/// Foundation does.
struct Video {
    num: u64,
    den: u64,
    next: u64,
    frames: u64,
}

impl Video {
    fn ms(&self, k: u64) -> u64 {
        k * 1000 * self.den / self.num
    }
}

impl MediaStream for Video {
    fn duration_ms(&self) -> u64 {
        self.frames * 1000 * self.den / self.num
    }
    fn seek(&mut self, ms: u64) -> Result<(), DecoderError> {
        let at_or_before = (0..self.frames).filter(|&k| self.ms(k) <= ms).count() as u64;
        self.next = at_or_before.saturating_sub(1) / 10 * 10;
        Ok(())
    }
}

impl VideoStream for Video {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        if self.next >= self.frames {
            return Ok(None);
        }
        let k = self.next;
        self.next += 1;
        let mut data = vec![0u8; 12];
        data[..2].copy_from_slice(&(k as u16).to_le_bytes());
        Ok(Some(DecodedVideoFrame {
            data,
            width: 4,
            height: 2,
            stride: 4,
            timestamp_ms: self.ms(k),
            pixel_format: PixelFormat::Nv12,
            gpu: None,
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
/// `enc(s)` on ch0 and `−enc(s)` on ch1. A seek is sample-accurate.
struct Audio {
    samples: i64,
    next: i64,
}

impl MediaStream for Audio {
    fn duration_ms(&self) -> u64 {
        (self.samples * 1000 / RATE) as u64
    }
    fn seek(&mut self, ms: u64) -> Result<(), DecoderError> {
        self.next = ms as i64 * RATE / 1000;
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

/// The decode producer as the paced pipeline runs it: the real decoder and
/// item builder, up to [`QUEUE_BOUND`] items ahead of the emit loop.
struct Producer {
    decoder: SplitSyncedDecoder,
    items: DecodeProducer,
    taps: DecodeTaps,
    queue: VecDeque<QueuedFrame>,
    eos: bool,
}

impl Producer {
    fn new(num: u64, den: u64, frames: u64, samples: i64) -> Self {
        let video = Video {
            num,
            den,
            next: 0,
            frames,
        };
        let audio = Audio { samples, next: 0 };
        Self {
            decoder: open_paced_decoder(Box::new(video), Box::new(audio))
                .expect("valid mock readers"),
            items: DecodeProducer::new(0),
            taps: DecodeTaps {
                preview: PreviewTap::new(Default::default(), "t".into()),
                stream: StreamTap::new("t".into(), 0),
            },
            queue: VecDeque::new(),
            eos: false,
        }
    }

    /// Decode ahead until the queue is full or the song's items ended.
    fn fill(&mut self) {
        while !self.eos && self.queue.len() < QUEUE_BOUND {
            match self.items.next_item(&mut self.decoder, &self.taps) {
                Ok(Some(item)) => self.queue.push_back(item),
                Ok(None) => self.eos = true,
                Err(e) => panic!("the mock readers never fail: {e}"),
            }
        }
    }

    /// A seek: the queue flushed, the decoder sought (`request_seek`).
    fn seek(&mut self, ms: u64) {
        self.queue.clear();
        self.eos = false;
        self.items.seek(&mut self.decoder, ms, 1, 0);
    }
}

/// A command the emit loop reads at a boundary's top.
#[derive(Clone, Copy)]
enum Cmd {
    /// The next song's Play (a skip): the song ends at once, no play-out.
    Skip,
    /// A seek to this position (ms).
    Seek(u64),
}

/// How a song left the emit loop.
#[derive(Debug, PartialEq)]
enum End {
    /// Its last item shown at this boundary, then the play-out.
    Natural(usize),
    /// A skip read at this boundary's top; the items still queued then.
    Skipped(usize, usize),
}

/// A song played through the emit loop: the pacer, the blocks it put out
/// (the play-out's included) and how the song ended.
struct Played {
    pacer: Pacer,
    rec: Rec,
    end: End,
}

/// Play `frames` frames at `num/den` fps against `samples` of audio as the
/// paced pipeline does, the commands of `script` read at their boundaries.
fn play(num: u64, den: u64, frames: u64, samples: i64, script: &[(usize, Cmd)]) -> Played {
    let mut producer = Producer::new(num, den, frames, samples);
    let (mut pacer, clk) = anchored();
    let mut rec = Rec::default();
    let mut audio_ended = false;
    for boundary in 1..=MAX_BOUNDARIES {
        match script.iter().find(|(at, _)| *at == boundary) {
            Some((_, Cmd::Skip)) => {
                let end = End::Skipped(boundary, producer.queue.len());
                return Played { pacer, rec, end };
            }
            Some((_, Cmd::Seek(ms))) => {
                producer.seek(*ms);
                audio_ended = false;
                pacer.anchor_seek();
            }
            None => {}
        }
        producer.fill();
        let target = pacer.next_boundary_100ns();
        pacer.prepare(target, || {
            let (frame, _, ended) = producer.queue.pop_front()?;
            audio_ended |= ended;
            Some(frame)
        });
        if audio_ended {
            pacer.audio_source_ended();
        }
        clk.set(target);
        pacer.service(|| None, &mut rec);
        producer.fill();
        if producer.eos && producer.queue.is_empty() && !pacer.has_pending() {
            pacer.play_out_audio(&mut rec, |_, until| clk.set(until));
            let end = End::Natural(boundary);
            return Played { pacer, rec, end };
        }
    }
    panic!("the song never ended");
}

/// A song played to its natural end; returns it and how many boundaries the
/// song itself took (its last item's).
fn play_to_the_end(num: u64, den: u64, frames: u64, samples: i64) -> (Played, usize) {
    let p = play(num, den, frames, samples, &[]);
    let End::Natural(boundaries) = p.end else {
        panic!("the song ended by a command: {:?}", p.end);
    };
    (p, boundaries)
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
/// real samples are the media ranges `segments` (`[from, to)`), each sample
/// once and in order, the next range right after the last sample of the one
/// before (a seek), with no silence between them; after the last range only
/// silence. Returns the index of the block holding the last real sample and
/// how many it holds.
fn assert_played(rec: &Rec, segments: &[(i64, i64)]) -> (usize, usize) {
    let mut seg = 0;
    let mut next = segments[0].0;
    let mut last = (0, 0);
    let ended = |seg: usize, next: i64| seg + 1 == segments.len() && next == segments[seg].1;
    for (i, (stamp, block)) in rec.blocks.iter().enumerate() {
        assert_eq!(*stamp, b(i as i64 + 1), "block {i} on its boundary");
        assert_eq!(block.len(), SPB as usize, "block {i} is a full boundary");
        let mut real = 0;
        for (j, &v) in block.iter().enumerate() {
            match dec(v) {
                Some(media) => {
                    if next == segments[seg].1 && seg + 1 < segments.len() {
                        seg += 1;
                        next = segments[seg].0;
                    }
                    assert_eq!(media, next, "block {i} sample {j}: media in order");
                    next += 1;
                    real += 1;
                }
                None => assert!(
                    ended(seg, next),
                    "block {i} sample {j}: silence inside the song (after media {next})"
                ),
            }
        }
        if real > 0 {
            last = (i, real);
        }
    }
    assert!(ended(seg, next), "every sample played (up to media {next})");
    last
}

/// [`assert_played`] for one range from media 0.
fn assert_every_sample_once_then_silence(rec: &Rec, samples: i64) -> (usize, usize) {
    assert_played(rec, &[(0, samples)])
}

/// The frame each block showed is `(0..frames)`, one per boundary, then
/// `frames − 1` held to the end (a 30 fps song: frame k is due at b(k + 1)).
fn assert_held_after_the_video(rec: &Rec, frames: u16) {
    let first: Vec<u16> = (0..frames).collect();
    assert_eq!(rec.shown[..frames as usize], first[..]);
    assert!(
        rec.shown[frames as usize..]
            .iter()
            .all(|&s| s == frames - 1),
        "the last frame is held for the audio past the video"
    );
}

#[test]
fn a_23_976_fps_song_of_equal_length_plays_every_sample_with_no_underrun() {
    // 301 frames = 602 602 samples. The audio ends inside the last frame's
    // own boundary: 1002 real samples, then padding. Before #257 that block
    // was an underrun.
    let samples = 602_602;
    let (p, song_boundaries) = play_to_the_end(24_000, 1001, 301, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (376, 1002)
    );
    assert_eq!(song_boundaries, 377);
    assert_eq!(p.rec.blocks.len(), 378, "one play-out boundary");
}

#[test]
fn a_25_fps_song_of_equal_length_ends_in_its_play_out_block_with_no_underrun() {
    // 301 frames = 577 920 samples; the last 320 leave on the play-out
    // boundary after the last frame's.
    let samples = 577_920;
    let (p, song_boundaries) = play_to_the_end(25, 1, 301, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (361, 320)
    );
    assert_eq!(song_boundaries, 361);
    assert_eq!(p.rec.blocks.len(), 362);
}

#[test]
fn a_60_fps_song_of_equal_length_plays_every_sample_with_no_underrun() {
    let samples = 480_000;
    let (p, song_boundaries) = play_to_the_end(60, 1, 600, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (299, 1600)
    );
    assert_eq!(song_boundaries, 301);
    assert_eq!(p.rec.blocks.len(), 302);
}

#[test]
fn audio_45_ms_longer_than_the_picture_plays_out_over_two_boundaries() {
    // 300 frames at 30 fps = 480 000 samples of picture; the audio runs 45 ms
    // (2160 samples) past it, inside the last frame's 250 ms lead. Before
    // #257 one standby block went out after the last frame and the last 560
    // samples were dropped.
    let samples = 482_160;
    let (p, song_boundaries) = play_to_the_end(30, 1, 300, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (301, 560)
    );
    assert_eq!(song_boundaries, 300);
    assert_eq!(p.rec.blocks.len(), 302, "two play-out boundaries");
}

#[test]
fn a_picture_50_ms_longer_than_its_audio_ends_in_silence_with_no_underrun() {
    // The audio ends 2400 samples before the picture: its last 800 samples
    // leave at b(299), then b(300) and the play-out are silence. Before #257
    // both blocks after the audio's last sample counted as underruns.
    let samples = 477_600;
    let (p, song_boundaries) = play_to_the_end(30, 1, 300, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (298, 800)
    );
    assert_eq!(song_boundaries, 300);
    assert_eq!(p.rec.blocks.len(), 301);
}

#[test]
fn audio_1_s_longer_than_the_picture_plays_on_the_held_last_frame() {
    // The audio runs 1 s past the picture (528 000 samples), far past the
    // last frame's 250 ms read-ahead. Before step 2 the decoder never read
    // the rest: 491 520 samples played and one `end` underrun. Now the held
    // last frame carries it on the grid (the last held step at b(322)) and
    // every sample plays: 0 underruns.
    let samples = 528_000;
    let (p, song_boundaries) = play_to_the_end(30, 1, 300, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (329, 1600)
    );
    assert_eq!(song_boundaries, 322);
    assert_eq!(p.rec.blocks.len(), 330);
    assert_held_after_the_video(&p.rec, 300);
}

#[test]
fn audio_8_s_longer_than_the_picture_plays_every_sample_on_the_held_last_frame() {
    // SNV's dub: its audio 7.8 s past the video. 8 s more here: every sample
    // plays, the last frame held from b(300) to the audio's end, 0 underruns.
    let samples = 480_000 + 8 * 48_000;
    let (p, song_boundaries) = play_to_the_end(30, 1, 300, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (539, 1600)
    );
    assert_eq!(song_boundaries, 532);
    assert_eq!(p.rec.blocks.len(), 540);
    assert_held_after_the_video(&p.rec, 300);
}

#[test]
fn a_23_976_fps_picture_is_held_on_its_own_cadence_for_8_s_more_audio() {
    // The held steps follow the source's 1001/24000 s frames (⌊k·41.708⌋ ms
    // past the last frame): every sample plays, the last frame (300) shown
    // from b(377) on.
    let samples = 602_602 + 8 * 48_000;
    let (p, song_boundaries) = play_to_the_end(24_000, 1001, 301, samples);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, samples),
        (616, 1002)
    );
    assert_eq!(song_boundaries, 610);
    assert_eq!(p.rec.blocks.len(), 617);
    assert!(p.rec.shown[376..].iter().all(|&s| s == 300));
}

#[test]
fn audio_past_the_bound_plays_15_s_on_the_held_frame_then_counts_one_end_underrun() {
    // A broken pair: the audio runs 20 s past the picture. The held frame
    // carries it 15 s (`AUDIO_PAST_VIDEO_MAX_MS`, the last step at b(750));
    // what the decoder read by then (1 212 416 samples, the last step's lead
    // + one packet) plays, the rest never does: ONE `end` underrun.
    let (p, song_boundaries) = play_to_the_end(30, 1, 300, 480_000 + 20 * 48_000);
    assert_eq!(underruns(&p.pacer), (0, 0, 1, 1));
    assert_eq!(
        assert_every_sample_once_then_silence(&p.rec, 1_212_416),
        (757, 1216)
    );
    assert_eq!(song_boundaries, 750);
    assert_eq!(p.rec.blocks.len(), 758);
    assert_held_after_the_video(&p.rec, 300);
}

#[test]
fn a_skip_during_the_held_tail_ends_the_song_at_once() {
    // 8 s of audio past the picture; the skip comes at b(400), 3.3 s into
    // the held tail, with the producer still a full queue ahead: the song
    // ends there, boundaries 1 … 399 played, nothing counted (a skip is no
    // loss), no play-out.
    let p = play(30, 1, 300, 480_000 + 8 * 48_000, &[(400, Cmd::Skip)]);
    assert_eq!(p.end, End::Skipped(400, QUEUE_BOUND));
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(p.rec.blocks.len(), 399);
    assert_eq!(assert_played(&p.rec, &[(0, 399 * 1600)]), (398, 1600));
    assert!(p.rec.shown[299..].iter().all(|&s| s == 299));
}

#[test]
fn a_seek_past_the_last_frame_plays_the_rest_of_the_audio_on_that_frame() {
    // A seek at b(100) to 14 s, past the picture's end (9.97 s) but inside
    // the audio (18 s): the video lands on keyframe 290 and runs out at frame
    // 299, which is held from b(100) on while the audio plays from 14 s
    // (media 672 000) to its end. 0 underruns.
    let p = play(
        30,
        1,
        300,
        480_000 + 8 * 48_000,
        &[(100, Cmd::Seek(14_000))],
    );
    assert_eq!(p.end, End::Natural(211));
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_played(&p.rec, &[(0, 99 * 1600), (672_000, 864_000)]),
        (218, 1600)
    );
    assert_eq!(p.rec.blocks.len(), 219);
    assert_eq!(p.rec.shown[97..100], [97, 98, 299]);
    assert!(p.rec.shown[99..].iter().all(|&s| s == 299));
}

#[test]
fn a_seek_back_during_the_held_tail_plays_the_picture_again() {
    // At b(400), in the held tail, a seek back to 5 s (frame 150): the
    // picture plays on from frame 150, then the tail is held again from its
    // last frame to the audio's end. Every sample after the seek plays once.
    let p = play(30, 1, 300, 480_000 + 8 * 48_000, &[(400, Cmd::Seek(5_000))]);
    assert_eq!(p.end, End::Natural(782));
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_eq!(
        assert_played(&p.rec, &[(0, 399 * 1600), (240_000, 864_000)]),
        (788, 1600)
    );
    assert_eq!(p.rec.blocks.len(), 789);
    assert_eq!(p.rec.shown[397..402], [299, 299, 150, 151, 152]);
    assert!(p.rec.shown[548..].iter().all(|&s| s == 299));
}
