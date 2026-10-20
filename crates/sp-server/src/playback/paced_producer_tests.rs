//! The paced decode producer's items (#147, #257), over the real
//! `SplitSyncedDecoder` and mock readers: each frame with its audio on the
//! song's 0-based origin, then past the video's end the held last frame (the
//! same allocation) with the audio still to play, until the audio ended;
//! the seek's origin and epoch; the picture a seek past the last frame holds;
//! the preview taps fed through the tail.

use std::time::Duration;

use sp_decoder::{
    AudioStream, DecodedAudioFrame, DecodedVideoFrame, DecoderError, MediaStream, PixelFormat,
    SplitSyncedDecoder, VideoStream,
};
use sp_gpu::FakeRing;

use super::{DecodeProducer, Held, QueuedFrame, to_paced_frame};
use crate::playback::picture::Picture;
use crate::playback::preview::PreviewTap;
use crate::playback::preview::preview_stream::{DecodeTaps, StreamTap, ViewerGuard};

/// A 4×2 NV12 picture (12 bytes) whose every byte is `mark`.
fn picture(mark: u8) -> Vec<u8> {
    vec![mark; 12]
}

/// How the mock video answers a seek.
#[derive(Clone, Copy)]
enum SeekAnswer {
    /// Lands on the last keyframe (every 3rd frame) at or before the position.
    Lands,
    /// Refuses every seek.
    Refuses,
    /// Lands at end-of-stream: no frame after any seek (a reader that lost
    /// its file).
    LosesTheFile,
}

/// A video reader of frames at the given ms (frame `k`'s bytes all `k + 1`),
/// 10 fps, answering a seek as `seeks`.
struct Video {
    ms: Vec<u64>,
    next: usize,
    seeks: SeekAnswer,
}

impl MediaStream for Video {
    fn duration_ms(&self) -> u64 {
        self.ms.last().copied().unwrap_or(0)
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        let at_or_before = self.ms.iter().take_while(|&&m| m <= position_ms).count();
        self.next = match self.seeks {
            SeekAnswer::Lands => at_or_before.saturating_sub(1) / 3 * 3,
            SeekAnswer::Refuses => return Err(DecoderError::Seek("refused".into())),
            SeekAnswer::LosesTheFile => self.ms.len(),
        };
        Ok(())
    }
}

impl VideoStream for Video {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let Some(&ms) = self.ms.get(self.next) else {
            return Ok(None);
        };
        let mark = (self.next + 1) as u8;
        self.next += 1;
        Ok(Some(DecodedVideoFrame {
            data: picture(mark),
            width: 4,
            height: 2,
            stride: 4,
            timestamp_ms: ms,
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
        (10, 1)
    }
}

/// An audio reader of stereo chunks at the given ms, each sample the chunk's
/// ms as a float; a seek lands on the first chunk at or after the position.
struct Audio {
    ms: Vec<u64>,
    next: usize,
}

impl MediaStream for Audio {
    fn duration_ms(&self) -> u64 {
        self.ms.last().copied().unwrap_or(0)
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.next = self.ms.iter().take_while(|&&m| m < position_ms).count();
        Ok(())
    }
}

impl AudioStream for Audio {
    fn next_samples(&mut self) -> Result<Option<DecodedAudioFrame>, DecoderError> {
        let Some(&ms) = self.ms.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        Ok(Some(chunk(ms)))
    }
    fn sample_rate(&self) -> u32 {
        48_000
    }
    fn channels(&self) -> u16 {
        2
    }
}

/// A stereo chunk at `ms` whose four samples are `ms` as a float.
fn chunk(ms: u64) -> DecodedAudioFrame {
    DecodedAudioFrame {
        data: vec![ms as f32; 4],
        channels: 2,
        sample_rate: 48_000,
        timestamp_ms: ms,
    }
}

/// A decoder (40 ms pairing) over frames and chunks at the given ms.
fn decoder(video: &[u64], audio: &[u64], refuse_seeks: bool) -> SplitSyncedDecoder {
    let seeks = if refuse_seeks {
        SeekAnswer::Refuses
    } else {
        SeekAnswer::Lands
    };
    decoder_answering(video, audio, seeks)
}

/// [`decoder`] with the video answering a seek as `seeks`.
fn decoder_answering(video: &[u64], audio: &[u64], seeks: SeekAnswer) -> SplitSyncedDecoder {
    SplitSyncedDecoder::new(
        Box::new(Video {
            ms: video.to_vec(),
            next: 0,
            seeks,
        }),
        Box::new(Audio {
            ms: audio.to_vec(),
            next: 0,
        }),
    )
    .expect("valid mock readers")
}

fn every(from: u64, to: u64, step: u64) -> Vec<u64> {
    (from..=to).step_by(step as usize).collect()
}

/// Preview taps nobody watches.
fn taps() -> DecodeTaps {
    DecodeTaps {
        preview: PreviewTap::new(Default::default(), "t".into()),
        stream: StreamTap::new("t".into(), 0),
    }
}

/// Every item until the producer hands out none (bounded).
fn items(p: &mut DecodeProducer, dec: &mut SplitSyncedDecoder) -> Vec<QueuedFrame> {
    let taps = taps();
    let mut out = Vec::new();
    for _ in 0..1_000 {
        match p.next_item(dec, &taps).expect("mocks never fail") {
            Some(item) => out.push(item),
            None => return out,
        }
    }
    panic!("the producer never stopped");
}

/// An item as `(pts ms, position ms, the audio's media ms, ended, picture
/// mark)`: the audio as each chunk's 0-based timecode in ms.
type Shape = (i64, u64, Vec<i64>, bool, u8);

/// One audio chunk of a paced frame: timecode, samples, channels, rate.
type ChunkShape<'a> = (Option<i64>, &'a [f32], u32, u32);

fn shape(item: &QueuedFrame) -> Shape {
    let (frame, ms, ended) = item;
    let audio = frame
        .audio
        .iter()
        .map(|a| a.timecode_100ns.expect("timed") / 10_000)
        .collect();
    (
        frame.pts_ns / 1_000_000,
        *ms,
        audio,
        *ended,
        frame.video.expect_cpu()[0],
    )
}

#[test]
fn a_frame_and_its_audio_go_out_on_the_songs_origin() {
    // A song that really starts at 1000 ms: a frame at 1400 is 400 ms in,
    // a chunk at 1350 is 350 ms in and one at 900 lies 100 ms before the
    // origin (a negative media time; the pacer aligns it away).
    let video = DecodedVideoFrame {
        data: picture(7),
        width: 4,
        height: 2,
        stride: 4,
        timestamp_ms: 1_400,
        pixel_format: PixelFormat::Nv12,
        gpu: None,
    };
    let frame = to_paced_frame(video, vec![chunk(1_350), chunk(900)], 1_000);
    assert_eq!(frame.pts_ns, 400_000_000);
    assert_eq!((frame.width, frame.height, frame.stride), (4, 2, 4));
    assert_eq!(&frame.video.expect_cpu()[..], &picture(7)[..]);
    let audio: Vec<ChunkShape> = frame
        .audio
        .iter()
        .map(|a| {
            (
                a.timecode_100ns,
                a.data.as_slice(),
                a.channels,
                a.sample_rate,
            )
        })
        .collect();
    assert_eq!(
        audio,
        vec![
            (Some(3_500_000), &[1_350.0f32; 4][..], 2, 48_000),
            (Some(-1_000_000), &[900.0f32; 4][..], 2, 48_000),
        ]
    );
    // A frame before the origin reads 0 (the pacer shows it at once).
    let early = DecodedVideoFrame {
        data: picture(1),
        width: 4,
        height: 2,
        stride: 4,
        timestamp_ms: 900,
        pixel_format: PixelFormat::Nv12,
        gpu: None,
    };
    assert_eq!(to_paced_frame(early, Vec::new(), 1_000).pts_ns, 0);
}

#[test]
fn past_the_video_the_last_frame_is_held_until_the_audio_ended() {
    // Frames 0, 100, 200; chunks every 100 ms to 500. Past the video the
    // producer hands out the held frame (the SAME allocation, never a copy)
    // at 300, 400, 500, each with its chunk; the 500 step carries the audio's
    // end, and then the song is over.
    let mut dec = decoder(&[0, 100, 200], &every(0, 500, 100), false);
    let mut p = DecodeProducer::new(0);
    let got = items(&mut p, &mut dec);
    let shapes: Vec<_> = got.iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            (0, 0, vec![0], false, 1),
            (100, 100, vec![100], false, 2),
            (200, 200, vec![200], false, 3),
            (300, 300, vec![300], false, 3),
            (400, 400, vec![400], false, 3),
            (500, 500, vec![500], true, 3),
        ]
    );
    let last_frame = &got[2].0.video;
    assert!(
        got[3..].iter().all(|i| i.0.video.ptr_eq(last_frame)),
        "a held step repeats the last frame's own buffer"
    );
    assert!(p.next_item(&mut dec, &taps()).unwrap().is_none());
}

#[test]
fn a_seek_moves_the_origin_and_the_epoch_and_the_tail_follows() {
    // Played into the tail, then a seek back to 100 (keyframe 0, the
    // decoder fast-forwards to 100): the items start again at pts 0 from
    // frame 100, under the new epoch, and the tail follows frame 200 anew.
    let mut dec = decoder(&[0, 100, 200], &every(0, 500, 100), false);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    for _ in 0..4 {
        p.next_item(&mut dec, &taps).unwrap().expect("an item");
    }
    assert_eq!(p.seek(&mut dec, 100, 7, 1), 100);
    assert_eq!(p.epoch(), 7);
    let shapes: Vec<_> = items(&mut p, &mut dec).iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            (0, 100, vec![0], false, 2),
            (100, 200, vec![100], false, 3),
            (200, 300, vec![200], false, 3),
            (300, 400, vec![300], false, 3),
            (400, 500, vec![400], true, 3),
        ]
    );
}

#[test]
fn a_refused_seek_plays_on_from_where_the_producer_was() {
    // The video refuses the seek: nothing moved, so the origin becomes the
    // position of the last item handed out (a held step's, here 300).
    let mut dec = decoder(&[0, 100, 200], &every(0, 500, 100), true);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    for _ in 0..4 {
        p.next_item(&mut dec, &taps).unwrap().expect("an item");
    }
    assert_eq!(p.seek(&mut dec, 50, 3, 1), 300);
    assert_eq!(p.epoch(), 3);
    let shapes: Vec<_> = items(&mut p, &mut dec).iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            (100, 400, vec![100], false, 3),
            (200, 500, vec![200], true, 3)
        ]
    );
}

#[test]
fn a_refused_seek_after_two_frames_plays_on_from_the_second_frame() {
    // Review: a frame handed out moves the producer's position. Two frames
    // out (0, 100), then a refused seek: the song plays on from 100 (the
    // pipeline reports `Seeked{100}`), never from its start.
    let mut dec = decoder(&[0, 100, 200], &every(0, 500, 100), true);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    for _ in 0..2 {
        p.next_item(&mut dec, &taps).unwrap().expect("a frame");
    }
    assert_eq!(p.seek(&mut dec, 400, 2, 1), 100);
    let shapes: Vec<_> = items(&mut p, &mut dec).iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            (100, 200, vec![100], false, 3),
            (200, 300, vec![200], false, 3),
            (300, 400, vec![300], false, 3),
            (400, 500, vec![400], true, 3),
        ]
    );
}

#[test]
fn a_seek_past_the_last_frame_holds_that_frame_from_the_seek() {
    // Frames 0 … 500, keyframes 0 and 300; chunks every 50 ms to 900. A
    // seek to 650 lands on keyframe 300 and decodes 300, 400, 500 before the
    // video ends: frame 500 (mark 6) is held from pts 0, and the audio from
    // the seek's position on, one frame period a step.
    let mut dec = decoder(&every(0, 500, 100), &every(0, 900, 50), false);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    p.next_item(&mut dec, &taps).unwrap().expect("frame 0");
    assert_eq!(p.seek(&mut dec, 650, 1, 1), 650);
    let shapes: Vec<_> = items(&mut p, &mut dec).iter().map(shape).collect();
    assert_eq!(
        shapes,
        vec![
            (0, 650, vec![0], false, 6),
            (100, 750, vec![50, 100], false, 6),
            (200, 850, vec![150, 200], false, 6),
            (300, 950, vec![250], true, 6),
        ]
    );
}

#[test]
fn a_seek_drops_the_held_picture_from_before_it() {
    // Review: a seek that lands no frame and decoded nothing before the
    // video's end (a reader that lost its file) leaves no picture to hold:
    // the song ends there. Frame 200, held from before the seek, is never
    // shown with the audio from the seek's position on.
    let mut dec = decoder_answering(
        &[0, 100, 200],
        &every(0, 900, 100),
        SeekAnswer::LosesTheFile,
    );
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    for _ in 0..4 {
        p.next_item(&mut dec, &taps).unwrap().expect("an item");
    }
    assert_eq!(p.seek(&mut dec, 100, 1, 1), 100);
    assert!(
        p.next_item(&mut dec, &taps).unwrap().is_none(),
        "no stale picture past the seek"
    );
}

#[test]
fn with_no_picture_ever_decoded_there_is_no_tail() {
    // A video with no frame at all: nothing to hold, so the song ends (its
    // audio past that point counts as the song's `end` underrun).
    let mut dec = decoder(&[], &every(0, 300, 100), false);
    let mut p = DecodeProducer::new(0);
    assert!(p.next_item(&mut dec, &taps()).unwrap().is_none());
}

#[test]
fn the_preview_gets_the_held_picture_and_its_audio_through_the_tail() {
    // The preview keeps a picture and its sound past the video's end: each
    // held step offers the held frame and its chunk to the stream tap.
    let mut dec = decoder(&[0, 100], &every(0, 300, 100), false);
    let mut p = DecodeProducer::new(0);
    let stream = StreamTap::new("t".into(), 0);
    let taps = DecodeTaps {
        preview: PreviewTap::new(Default::default(), "t".into()),
        stream: stream.clone(),
    };
    let (_viewer, _relay) = ViewerGuard::subscribe(&stream);
    let video = stream.shared().video_receiver();
    let audio = stream.shared().audio_receiver();
    for _ in 0..2 {
        p.next_item(&mut dec, &taps).unwrap().expect("a frame");
    }
    while video.try_recv().is_ok() {}
    while audio.try_recv().is_ok() {}
    let (_, ms, _) = p.next_item(&mut dec, &taps).unwrap().expect("a held step");
    assert_eq!(ms, 200);
    assert!(video.try_recv().is_ok(), "the held picture was offered");
    let block = audio.try_recv().expect("its chunk was offered");
    assert_eq!(block.samples, vec![200.0f32; 4]);
}

#[test]
fn a_held_ring_frame_gives_the_preview_its_audio_only_and_is_counted() {
    // #279 P2: past the video's end the held picture is a ring frame (P3a's
    // hardware decode). The preview taps cannot read it yet (P3b): each
    // held step offers its chunk alone and counts the miss, never a panic;
    // the queue item carries the ring frame itself (an `Arc` bump).
    let mut dec = decoder(&[0, 100], &every(0, 300, 100), false);
    let mut p = DecodeProducer::new(0);
    let stream = StreamTap::new("t".into(), 0);
    let taps = DecodeTaps {
        preview: PreviewTap::new(Default::default(), "t".into()),
        stream: stream.clone(),
    };
    let (_viewer, _relay) = ViewerGuard::subscribe(&stream);
    let video = stream.shared().video_receiver();
    let audio = stream.shared().audio_receiver();
    for _ in 0..2 {
        p.next_item(&mut dec, &taps).unwrap().expect("a frame");
    }
    let ring = FakeRing::new(1);
    let frame = ring.frame(4, 2, Duration::ZERO).expect("a free slot");
    p.held = Some(Held {
        width: 4,
        height: 2,
        stride: 4,
        video: Picture::Gpu(frame.clone()),
    });
    while video.try_recv().is_ok() {}
    while audio.try_recv().is_ok() {}
    for (step, ms) in [(1, 200), (2, 300)] {
        let (item, at, _) = p.next_item(&mut dec, &taps).unwrap().expect("a held step");
        assert_eq!(at, ms);
        assert!(video.try_recv().is_err(), "no picture offered at {ms}");
        let block = audio.try_recv().expect("its chunk was offered");
        assert_eq!(block.samples, vec![ms as f32; 4]);
        assert_eq!(p.preview_misses(), step, "one miss per held step");
        assert!(
            item.video.gpu().is_some_and(|g| g.ptr_eq(&frame)),
            "the ring frame itself"
        );
    }
}

/// #277: an early end of the video (a child module over these mocks).
#[path = "paced_producer_tests_early.rs"]
mod early;

/// #279 P3a: GPU-resident frames (a child module over these mocks).
#[path = "paced_producer_tests_gpu.rs"]
mod gpu;
