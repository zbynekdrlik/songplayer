//! #318: the producer while the next picture is late — the lateness rule,
//! `poll_item`'s late step (the held picture again, the same allocation,
//! with the audio up to its time plus the lead), nothing to hold before the
//! first frame, and GPU-resident frames through the real reader thread
//! (`sp_decoder::ThreadedVideo`): a late step holds the ring frame itself,
//! and the frames dropped behind the audio line free their ring slots.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use sp_decoder::{
    DecodedVideoFrame, DecoderError, FramePoll, MediaStream, SplitSyncedDecoder, ThreadedVideo,
    VideoStream,
};
use sp_gpu::FakeRing;

use super::{Audio, SeekAnswer, Video, every, taps};
use crate::playback::paced_producer::{DecodeProducer, Next, picture_is_late};

/// A bound no correct step comes near.
const BOUND: Duration = Duration::from_secs(20);

#[test]
fn the_next_picture_is_late_only_when_due_and_the_queue_has_room() {
    // 33 ms period: due from 90 % of it, 29 700 µs.
    assert!(
        !picture_is_late(0, None, 33, false),
        "no pop yet: the pre-roll"
    );
    assert!(!picture_is_late(0, Some(29_699), 33, false), "not due yet");
    assert!(picture_is_late(0, Some(29_700), 33, false), "due");
    assert!(picture_is_late(0, Some(u64::MAX), 33, false), "no overflow");
    assert!(
        !picture_is_late(1, Some(40_000), 33, false),
        "an item still queued"
    );
    // Once a late run began the queue is topped up to the depth.
    assert!(picture_is_late(1, Some(40_000), 33, true));
    assert!(picture_is_late(2, Some(40_000), 33, true));
    assert!(
        !picture_is_late(3, Some(40_000), 33, true),
        "the depth reached"
    );
    assert!(
        !picture_is_late(2, Some(29_699), 33, true),
        "a run still waits for due"
    );
    // 41 ms (23.976 fps): due from 36 900 µs.
    assert!(!picture_is_late(0, Some(36_899), 41, false));
    assert!(picture_is_late(0, Some(36_900), 41, false));
}

/// [`Video`] whose polls answer from `polls` first (`false` = not ready).
struct Polled {
    frames: Video,
    polls: Vec<bool>,
}

impl MediaStream for Polled {
    fn duration_ms(&self) -> u64 {
        self.frames.duration_ms()
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.frames.seek(position_ms)
    }
}

impl VideoStream for Polled {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        self.frames.next_frame()
    }
    fn poll_frame(&mut self, _wait: Duration) -> Result<FramePoll, DecoderError> {
        if !self.polls.is_empty() && !self.polls.remove(0) {
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
        (10, 1)
    }
}

/// A decoder (40 ms lead) over frames at 0, 100, …, 500 and chunks every
/// 100 ms to 900, the polls answering `polls` first.
fn polled(polls: &[bool]) -> SplitSyncedDecoder {
    SplitSyncedDecoder::new(
        Box::new(Polled {
            frames: Video {
                ms: every(0, 500, 100),
                next: 0,
                seeks: SeekAnswer::Lands,
            },
            polls: polls.to_vec(),
        }),
        Box::new(Audio {
            ms: every(0, 900, 100),
            next: 0,
        }),
    )
    .expect("valid mock readers")
}

/// The item a poll hands out, or a panic naming what came instead.
fn item(next: Next) -> crate::playback::paced_producer::QueuedFrame {
    match next {
        Next::Item(item) => item,
        other => panic!("an item: {other:?}"),
    }
}

/// The audio times (ms) of an item.
fn audio_ms(item: &crate::playback::paced_producer::QueuedFrame) -> Vec<i64> {
    item.0
        .audio
        .iter()
        .map(|a| a.timecode_100ns.expect("timed") / 10_000)
        .collect()
}

#[test]
fn a_late_step_holds_the_last_picture_with_its_audio() {
    let mut dec = polled(&[true, false, false]);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    let zero = Some(Duration::ZERO);
    let first = item(p.poll_item(&mut dec, &taps, zero, false).unwrap());
    assert_eq!((first.0.pts_ns, first.1), (0, 0));
    // Not late: the producer looks again.
    assert!(matches!(
        p.poll_item(&mut dec, &taps, zero, false).unwrap(),
        Next::NotReady
    ));
    // Late: the held frame 0 at 100 ms with the audio up to 140 ms.
    let step = item(p.poll_item(&mut dec, &taps, zero, true).unwrap());
    assert_eq!((step.0.pts_ns, step.1, step.2), (100_000_000, 100, false));
    assert!(step.0.video.ptr_eq(&first.0.video), "the same allocation");
    assert_eq!(audio_ms(&step), [100]);
    // The picture is back: frame 1 (100 ms) lies on the line and is
    // dropped, frame 2 shows with the audio from where the step left off.
    let back = item(p.poll_item(&mut dec, &taps, zero, true).unwrap());
    assert_eq!(back.1, 200);
    assert_eq!(back.0.video.expect_cpu()[0], 3, "frame 2's bytes");
    assert_eq!(audio_ms(&back), [200]);
    assert_eq!(dec.late_counts(), (1, 1));
}

#[test]
fn nothing_is_held_before_the_first_frame() {
    let mut dec = polled(&[false, false]);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    let zero = Some(Duration::ZERO);
    for _ in 0..2 {
        assert!(matches!(
            p.poll_item(&mut dec, &taps, zero, true).unwrap(),
            Next::NotReady
        ));
    }
    assert_eq!(dec.late_counts(), (0, 0));
    let first = item(p.poll_item(&mut dec, &taps, zero, true).unwrap());
    assert_eq!(first.1, 0);
}

#[test]
fn no_wait_is_the_old_blocking_read() {
    let mut dec = polled(&[false, false, false]);
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    // `None`: the plain read, the polls never asked.
    let first = item(p.poll_item(&mut dec, &taps, None, true).unwrap());
    assert_eq!(first.1, 0);
    assert_eq!(
        p.next_item(&mut dec, &taps).unwrap().expect("frame 1").1,
        100
    );
}

/// A gate a read waits at while it is closed.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
    }
}

/// Frames in ring slots (as a GPU-resident reader hands them over), the read
/// of frame `block_at` waiting at the gate.
struct RingVideo {
    frames: Video,
    ring: FakeRing,
    block_at: usize,
    gate: Arc<Gate>,
}

impl MediaStream for RingVideo {
    fn duration_ms(&self) -> u64 {
        self.frames.duration_ms()
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        self.frames.seek(position_ms)
    }
}

impl VideoStream for RingVideo {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        if self.frames.next == self.block_at {
            self.gate.wait();
        }
        let Some(mut frame) = self.frames.next_frame()? else {
            return Ok(None);
        };
        let slot = self
            .ring
            .frame(frame.width, frame.height, Duration::ZERO)
            .map_err(|e| DecoderError::BufferLock(e.to_string()))?;
        frame.gpu = Some(slot);
        frame.data = Vec::new();
        Ok(Some(frame))
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

#[test]
fn gpu_frames_cross_the_reader_thread_and_a_late_step_holds_the_ring_frame() {
    let ring = FakeRing::new(8);
    let gate = Arc::new(Gate::default());
    let reader = RingVideo {
        frames: Video {
            ms: every(0, 500, 100),
            next: 0,
            seeks: SeekAnswer::Lands,
        },
        ring: ring.clone(),
        block_at: 1,
        gate: Arc::clone(&gate),
    };
    let (video, ()) = ThreadedVideo::spawn(
        |body| {
            crate::playback::decode_thread::spawn_decode_thread("paced-video-t".into(), body)
                .map(drop)
        },
        move || Ok((Box::new(reader) as Box<dyn VideoStream>, ())),
    )
    .expect("the reader opens");
    let mut dec = SplitSyncedDecoder::new(
        Box::new(video),
        Box::new(Audio {
            ms: every(0, 900, 100),
            next: 0,
        }),
    )
    .expect("valid readers");
    let mut p = DecodeProducer::new(0);
    let taps = taps();
    let first = item(p.poll_item(&mut dec, &taps, Some(BOUND), false).unwrap());
    assert!(
        first.0.video.gpu().is_some(),
        "a GPU picture across the thread"
    );
    // Frame 1's read blocks on the reader thread: not ready within 20 ms.
    let wait = Some(Duration::from_millis(20));
    assert!(matches!(
        p.poll_item(&mut dec, &taps, wait, false).unwrap(),
        Next::NotReady
    ));
    let mut steps = Vec::new();
    for _ in 0..2 {
        steps.push(item(p.poll_item(&mut dec, &taps, wait, true).unwrap()));
    }
    for step in &steps {
        assert!(step.0.video.ptr_eq(&first.0.video), "the ring frame itself");
    }
    assert_eq!((steps[0].1, steps[1].1), (100, 200));
    gate.open();
    // Frames 1 and 2 lie on the line (200 ms): decoded into slots, dropped.
    let back = item(p.poll_item(&mut dec, &taps, Some(BOUND), true).unwrap());
    assert_eq!(back.1, 300);
    assert!(back.0.video.gpu().is_some());
    assert_eq!(dec.late_counts(), (2, 2));
    drop((first, steps, back, p));
    drop(dec);
    // Every slot comes back once the reader thread has let its last read go.
    for _ in 0..2_000 {
        if ring.stats().in_use == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("ring slots still in use: {:?}", ring.stats().in_use);
}
