//! #279 P3a: GPU-resident frames through the producer: a frame the reader
//! copied into its pipeline's ring travels as `Picture::Gpu` (the ring
//! frame itself, no bytes); the preview taps get its audio only, a viewer
//! watching then is a counted miss; the frames' kind is reported at the
//! song's first frame and at a change mid-song (a fall back to software).

use std::time::Duration;

use sp_decoder::{
    DecodedVideoFrame, DecoderError, MediaStream, PixelFormat, SplitSyncedDecoder, VideoStream,
};
use sp_gpu::FakeRing;

use super::{Audio, SeekAnswer, Video, every, taps};
use crate::playback::paced_producer::{DecodeProducer, KindChange, to_paced_frame};
use crate::playback::preview::PreviewTap;
use crate::playback::preview::preview_stream::{DecodeTaps, StreamTap, ViewerGuard};

/// [`Video`] whose first `on_gpu` frames lie in ring slots, as a
/// GPU-resident reader hands them over (no bytes, a stride of one chroma
/// row); the rest in CPU memory (its fall back to software).
struct OnGpu {
    frames: Video,
    ring: FakeRing,
    on_gpu: usize,
    handed: usize,
}

impl MediaStream for OnGpu {
    fn duration_ms(&self) -> u64 {
        self.frames.duration_ms()
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.frames.seek(position_ms)
    }
}

impl VideoStream for OnGpu {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        let Some(mut frame) = self.frames.next_frame()? else {
            return Ok(None);
        };
        if self.handed < self.on_gpu {
            let slot = self
                .ring
                .frame(frame.width, frame.height, Duration::ZERO)
                .map_err(|e| DecoderError::BufferLock(e.to_string()))?;
            frame.gpu = Some(slot);
            frame.data = Vec::new();
        }
        self.handed += 1;
        Ok(Some(frame))
    }
    fn width(&self) -> u32 {
        self.frames.width()
    }
    fn height(&self) -> u32 {
        self.frames.height()
    }
    fn frame_rate(&self) -> (u32, u32) {
        self.frames.frame_rate()
    }
}

/// A decoder whose first `on_gpu` frames (at 0, 100, 200, 300 ms) lie on
/// the GPU, with a chunk every 100 ms.
fn decoder(on_gpu: usize) -> SplitSyncedDecoder {
    SplitSyncedDecoder::new(
        Box::new(OnGpu {
            frames: Video {
                ms: every(0, 300, 100),
                next: 0,
                seeks: SeekAnswer::Lands,
            },
            ring: FakeRing::new(16),
            on_gpu,
            handed: 0,
        }),
        Box::new(Audio {
            ms: every(0, 300, 100),
            next: 0,
        }),
    )
    .expect("valid mock readers")
}

#[test]
fn a_ring_frame_travels_as_a_gpu_picture() {
    let ring = FakeRing::new(1);
    let slot = ring.frame(4, 2, Duration::ZERO).expect("a free slot");
    let video = DecodedVideoFrame {
        data: Vec::new(),
        width: 4,
        height: 2,
        stride: 4,
        timestamp_ms: 1_400,
        pixel_format: PixelFormat::Nv12,
        gpu: Some(slot.clone()),
    };
    let frame = to_paced_frame(video, Vec::new(), 1_000);
    assert!(
        frame.video.gpu().is_some_and(|g| g.ptr_eq(&slot)),
        "the ring frame itself"
    );
    assert_eq!(frame.video.len(), 0, "no bytes in CPU memory");
    assert_eq!(
        (frame.pts_ns, frame.width, frame.height),
        (400_000_000, 4, 2)
    );
}

#[test]
fn gpu_frames_give_a_watched_preview_their_audio_and_count_a_miss() {
    let mut dec = decoder(2);
    let mut p = DecodeProducer::new(0);
    let stream = StreamTap::new("t".into(), 0);
    let taps = DecodeTaps {
        preview: PreviewTap::new(Default::default(), "t".into()),
        stream: stream.clone(),
    };
    let (_viewer, _relay) = ViewerGuard::subscribe(&stream);
    assert!(taps.wants_picture(), "a viewer watches");
    let video = stream.shared().video_receiver();
    let audio = stream.shared().audio_receiver();

    let (first, ms, _) = p.next_item(&mut dec, &taps).unwrap().expect("frame 0");
    assert_eq!(ms, 0);
    assert!(first.video.gpu().is_some(), "a GPU picture");
    assert!(video.try_recv().is_err(), "no picture offered");
    assert_eq!(
        audio.try_recv().expect("its chunk").samples,
        vec![0.0f32; 4]
    );
    assert_eq!(p.preview_misses(), 1);
    assert_eq!(
        p.take_kind_change(),
        Some(KindChange {
            gpu: true,
            first: true,
            size: (4, 2)
        })
    );
    assert_eq!(p.take_kind_change(), None, "taken once");

    let (second, _, _) = p.next_item(&mut dec, &taps).unwrap().expect("frame 1");
    assert!(second.video.gpu().is_some());
    assert_eq!(p.preview_misses(), 2);
    assert_eq!(p.take_kind_change(), None, "the same kind");

    // The reader fell back to software: a CPU picture, offered.
    let (third, _, _) = p.next_item(&mut dec, &taps).unwrap().expect("frame 2");
    assert_eq!(third.video.expect_cpu()[0], 3, "frame 2's bytes");
    assert!(video.try_recv().is_ok(), "the CPU picture is offered");
    assert_eq!(p.preview_misses(), 2, "no miss");
    assert_eq!(
        p.take_kind_change(),
        Some(KindChange {
            gpu: false,
            first: false,
            size: (4, 2)
        }),
        "a change mid-song"
    );
}

#[test]
fn gpu_frames_nobody_watches_count_no_miss() {
    let mut dec = decoder(4);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    assert!(!taps.wants_picture());
    for _ in 0..4 {
        let (item, _, _) = p.next_item(&mut dec, &taps).unwrap().expect("a frame");
        assert!(item.video.gpu().is_some());
    }
    assert_eq!(p.preview_misses(), 0);
    let change = p.take_kind_change().expect("the first frame's kind");
    assert!(change.gpu && change.first);
}

#[test]
fn a_cpu_songs_first_frame_reports_its_kind_too() {
    let mut dec = decoder(0);
    let mut p = DecodeProducer::new(0);
    assert_eq!(p.take_kind_change(), None, "before any frame");
    p.next_item(&mut dec, &taps()).unwrap().expect("frame 0");
    assert_eq!(
        p.take_kind_change(),
        Some(KindChange {
            gpu: false,
            first: true,
            size: (4, 2)
        })
    );
    p.next_item(&mut dec, &taps()).unwrap().expect("frame 1");
    assert_eq!(p.take_kind_change(), None);
}
