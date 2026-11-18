//! #318 — a video read that blocks mid-song never starves the audio.
//!
//! On SNV (13.11.2026) a GPU display reconfiguration held Media Foundation's
//! D3D11 decoder 0.5–0.75 s; the decode producer read no audio meanwhile, the
//! emit thread ran through its 12 queued frames and the 250 ms lead, and the
//! song's audio grid ran dry (`underruns_mid`). Now the reader sits on its own
//! thread, the producer waits for the next picture only until it is due
//! (`picture_is_late`), and hands out late steps (the held picture with the
//! audio up to the step's time plus the lead) until pictures come back; the
//! pictures at or before the last late step are dropped.
//!
//! These tests drive the REAL paced decoder, the REAL producer items
//! (`DecodeProducer::poll_item`) and the REAL lateness rule on the virtual
//! clock of the emit loop (one boundary = 33 333 µs), the pure [`Pacer`] and
//! the emit loop's order, as `pacer_tests_song_end.rs` does. The mock video
//! is "not ready" (`FramePoll::NotReady`, a read still running on its thread)
//! for one frame during a span of boundaries.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use sp_decoder::{DecodedVideoFrame, DecoderError, FramePoll, MediaStream, VideoStream};

use super::{
    Audio, Cmd, End, MAX_BOUNDARIES, QUEUE_BOUND, RATE, Rec, SPB, Video, anchored, assert_played,
    dec, underruns,
};
use crate::playback::paced_producer::{DecodeProducer, Next, QueuedFrame, picture_is_late};
use crate::playback::pacer::{Pacer, open_paced_decoder};
use crate::playback::preview::PreviewTap;
use crate::playback::preview::preview_stream::{DecodeTaps, StreamTap};

/// One boundary of the 30 fps grid in µs (the virtual clock's step).
const SLOT_US: u64 = 33_333;

/// [`Video`] whose frame `frame` is not ready while the emit loop's boundary
/// (`now`) is before `until`: the reader thread is still inside its read.
struct Stalling {
    frames: Video,
    frame: u64,
    until: usize,
    now: Arc<AtomicUsize>,
}

impl MediaStream for Stalling {
    fn duration_ms(&self) -> u64 {
        self.frames.duration_ms()
    }
    fn seek(&mut self, ms: u64) -> Result<(), DecoderError> {
        self.frames.seek(ms)
    }
}

impl VideoStream for Stalling {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        self.frames.next_frame()
    }
    fn poll_frame(&mut self, _wait: Duration) -> Result<FramePoll, DecoderError> {
        if self.frames.next == self.frame && self.now.load(Ordering::SeqCst) < self.until {
            return Ok(FramePoll::NotReady);
        }
        Ok(FramePoll::Ready(self.frames.next_frame()?))
    }
    fn width(&self) -> u32 {
        4
    }
    fn height(&self) -> u32 {
        2
    }
    fn frame_rate(&self) -> (u32, u32) {
        self.frames.frame_rate()
    }
}

/// The decode producer as the paced pipeline runs it over a stalling video:
/// at each look the lateness rule from the queue and the boundary of the last
/// pop, then one `poll_item`; a `NotReady` ends the look.
struct Producer {
    decoder: sp_decoder::SplitSyncedDecoder,
    items: DecodeProducer,
    taps: DecodeTaps,
    queue: std::collections::VecDeque<QueuedFrame>,
    eos: bool,
    period_ms: u64,
}

impl Producer {
    fn fill(&mut self, boundary: usize, last_pop: Option<usize>) {
        while !self.eos && self.queue.len() < QUEUE_BOUND {
            let since = last_pop.map(|at| (boundary - at) as u64 * SLOT_US);
            let in_run = self.decoder.in_late_run();
            let late = picture_is_late(self.queue.len(), since, self.period_ms, in_run);
            let next =
                self.items
                    .poll_item(&mut self.decoder, &self.taps, Some(Duration::ZERO), late);
            match next.expect("the mock readers never fail") {
                Next::Item(item) => self.queue.push_back(item),
                Next::End => self.eos = true,
                Next::NotReady => return,
            }
        }
    }
}

/// A song played over a video whose frame `stall_frame` is not ready from
/// the start until boundary `until`.
struct Played {
    pacer: Pacer,
    rec: Rec,
    end: End,
    /// Late steps handed out and late frames dropped.
    late: (u64, u64),
}

fn play_stalled(
    num: u64,
    den: u64,
    frames: u64,
    samples: i64,
    stall_frame: u64,
    until: usize,
    script: &[(usize, Cmd)],
) -> Played {
    let now = Arc::new(AtomicUsize::new(0));
    let video = Stalling {
        frames: Video {
            num,
            den,
            next: 0,
            frames,
        },
        frame: stall_frame,
        until,
        now: Arc::clone(&now),
    };
    let decoder = open_paced_decoder(Box::new(video), Box::new(Audio { samples, next: 0 }))
        .expect("valid mock readers");
    let mut producer = Producer {
        period_ms: decoder.frame_period_ms(),
        decoder,
        items: DecodeProducer::new(0),
        taps: DecodeTaps {
            preview: PreviewTap::new(Default::default(), "t".into()),
            stream: StreamTap::new("t".into(), 0),
        },
        queue: Default::default(),
        eos: false,
    };
    let (mut pacer, clk) = anchored();
    let mut rec = Rec::default();
    let mut audio_ended = false;
    let mut last_pop = None;
    for boundary in 1..=MAX_BOUNDARIES {
        now.store(boundary, Ordering::SeqCst);
        match script.iter().find(|(at, _)| *at == boundary) {
            Some((_, Cmd::Skip)) => {
                let end = End::Skipped(boundary, producer.queue.len());
                let late = producer.decoder.late_counts();
                return Played {
                    pacer,
                    rec,
                    end,
                    late,
                };
            }
            Some((_, Cmd::Seek(ms))) => {
                producer.queue.clear();
                producer.eos = false;
                producer.items.seek(&mut producer.decoder, *ms, 1, 0);
                audio_ended = false;
                pacer.anchor_seek();
            }
            None => {}
        }
        producer.fill(boundary, last_pop);
        let target = pacer.next_boundary_100ns();
        pacer.prepare(target, || {
            let (frame, _, ended) = producer.queue.pop_front()?;
            last_pop = Some(boundary);
            audio_ended |= ended;
            Some(frame)
        });
        if audio_ended {
            pacer.audio_source_ended();
        }
        clk.set(target);
        pacer.service(|| None, &mut rec);
        producer.fill(boundary, last_pop);
        if producer.eos && producer.queue.is_empty() && !pacer.has_pending() {
            pacer.play_out_audio(&mut rec, |_, until| clk.set(until));
            let late = producer.decoder.late_counts();
            let end = End::Natural(boundary);
            return Played {
                pacer,
                rec,
                end,
                late,
            };
        }
    }
    panic!("the song never ended");
}

/// The frames shown never go back, and once the picture is back (from
/// boundary `from`) each shown frame lies within two source frames of the
/// audio its boundary plays: no picture behind its audio line.
fn assert_picture_on_the_audio_line(rec: &Rec, from: usize, num: u64, den: u64) {
    for w in rec.shown.windows(2) {
        assert!(w[0] <= w[1], "a frame went back: {w:?}");
    }
    let period_ms = 1000 * den as i64 / num as i64;
    for (i, (_, block)) in rec.blocks.iter().enumerate().skip(from) {
        let Some(media) = block.iter().find_map(|&v| dec(v)) else {
            continue;
        };
        let audio_ms = media * 1000 / RATE;
        let frame_ms = i64::from(rec.shown[i]) * 1000 * den as i64 / num as i64;
        assert!(
            (frame_ms - audio_ms).abs() <= 2 * period_ms,
            "boundary {i}: frame at {frame_ms} ms, audio at {audio_ms} ms"
        );
    }
}

#[test]
fn a_1_s_video_stall_mid_song_plays_every_sample_with_no_underrun() {
    // 300 frames at 30 fps, 10 s of audio. Frame 150's read blocks from the
    // start until boundary 182, ~1 s after the emit loop needed it (frame
    // 150 is due at b(151), the queue had run ahead). Before #318 the
    // producer pushed nothing meanwhile: the 12 queued frames and the
    // 250 ms lead ran out and the grid zero-filled (`underruns_mid`). Now the
    // held frame 149 carries the audio, then the picture catches up.
    let samples = 480_000;
    let p = play_stalled(30, 1, 300, samples, 150, 182, &[]);
    assert!(matches!(p.end, End::Natural(_)), "{:?}", p.end);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_played(&p.rec, &[(0, samples)]);
    let held = p.rec.shown.iter().filter(|&&s| s == 149).count();
    assert!(
        held >= 25,
        "frame 149 held through the stall: {held} boundaries"
    );
    assert!(p.late.0 >= 20, "late steps carried the audio: {:?}", p.late);
    assert!(
        p.late.1 >= 20,
        "the frames behind the audio line dropped: {:?}",
        p.late
    );
    assert_eq!(
        p.rec.shown.last(),
        Some(&299),
        "the picture caught up to its end"
    );
    assert_picture_on_the_audio_line(&p.rec, 190, 30, 1);
    for (i, (_, block)) in p.rec.blocks.iter().enumerate() {
        assert_eq!(block.len(), SPB as usize, "block {i} is a full boundary");
    }
}

#[test]
fn a_1_s_stall_of_a_23_976_fps_song_plays_every_sample_with_no_underrun() {
    let samples = 602_602;
    // Frame 120 is due at b(151); its read blocks until b(185).
    let p = play_stalled(24_000, 1001, 301, samples, 120, 185, &[]);
    assert!(matches!(p.end, End::Natural(_)), "{:?}", p.end);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_played(&p.rec, &[(0, samples)]);
    assert!(p.late.0 > 0 && p.late.1 > 0, "{:?}", p.late);
    assert_eq!(p.rec.shown.last(), Some(&300));
    assert_picture_on_the_audio_line(&p.rec, 195, 24_000, 1001);
}

#[test]
fn a_1_s_stall_of_a_60_fps_song_plays_every_sample_with_no_underrun() {
    // Two source frames a boundary: the late run tops the queue up
    // (`LATE_QUEUE_DEPTH`), one late step a pop would starve the audio.
    let samples = 480_000;
    let p = play_stalled(60, 1, 600, samples, 300, 185, &[]);
    assert!(matches!(p.end, End::Natural(_)), "{:?}", p.end);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_played(&p.rec, &[(0, samples)]);
    assert!(p.late.0 > 0 && p.late.1 > 0, "{:?}", p.late);
    assert_eq!(p.rec.shown.last(), Some(&599));
    assert_picture_on_the_audio_line(&p.rec, 195, 60, 1);
}

#[test]
fn a_stall_into_the_video_end_holds_on_and_plays_the_rest_of_the_audio() {
    // The read of the LAST frame (299) blocks until b(330), past the audio's
    // own end: the late steps carry every remaining sample, the picture is
    // never back (frame 299 lies before the late line and is dropped), the
    // video ends, nothing is counted.
    let samples = 480_000;
    let p = play_stalled(30, 1, 300, samples, 299, 330, &[]);
    assert!(matches!(p.end, End::Natural(_)), "{:?}", p.end);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_played(&p.rec, &[(0, samples)]);
    assert!(p.rec.shown.iter().all(|&s| s <= 299));
}

#[test]
fn a_seek_during_the_stall_plays_on_from_the_new_place() {
    // Frame 150 is late from b(150); at b(165), inside the stall, a seek to
    // 7 s (frame 210): the late run ends with it, the picture plays from 210,
    // every sample after the seek once, no underrun.
    let samples = 480_000;
    let p = play_stalled(30, 1, 300, samples, 150, 400, &[(165, Cmd::Seek(7_000))]);
    assert!(matches!(p.end, End::Natural(_)), "{:?}", p.end);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    let seek_at = 164 * 1600;
    assert_played(&p.rec, &[(0, seek_at), (336_000, samples)]);
    let after = p
        .rec
        .shown
        .iter()
        .position(|&s| s >= 210)
        .expect("the new place");
    assert!(p.rec.shown[after..].windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn a_skip_during_the_stall_ends_the_song_at_once() {
    let p = play_stalled(30, 1, 300, 480_000, 150, 400, &[(170, Cmd::Skip)]);
    assert!(matches!(p.end, End::Skipped(170, _)), "{:?}", p.end);
    assert_eq!(underruns(&p.pacer), (0, 0, 0, 0));
    assert_played(&p.rec, &[(0, 169 * 1600)]);
}
