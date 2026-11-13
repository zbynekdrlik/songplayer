//! #318: the video reader on its own thread — frames in order, the reader
//! opened, read, seeked and dropped on ITS thread, a blocked read answered
//! `NotReady` within the wait, a seek that drops what was read before it (a
//! refused one keeps it), and a drop that never waits for a blocked read.
//! Every wait is bounded (`recv_timeout`), never a sleep.

use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::ThreadedVideo;
use crate::error::DecoderError;
use crate::stream::{FramePoll, MediaStream, VideoStream};
use crate::types::{DecodedVideoFrame, PixelFormat};

/// A bound no correct step comes near.
const BOUND: Duration = Duration::from_secs(20);

/// A gate a read waits at while it is closed.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn closed() -> Arc<Self> {
        Arc::new(Self::default())
    }
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

/// What the reader did, and on which thread.
#[derive(Debug, PartialEq, Eq, Clone)]
enum Did {
    Opened(String),
    Read(String, u64),
    Sought(String, u64),
    Dropped(String),
}

fn thread_name() -> String {
    std::thread::current().name().unwrap_or("?").to_owned()
}

/// Frames at 0, 100, … ms (frame `k`'s bytes all `k`); the read of frame
/// `block_at` waits at `gate`; a seek lands on the frame at the position
/// (refused while `refuse` holds).
struct Reader {
    frames: u64,
    next: u64,
    block_at: Option<u64>,
    gate: Arc<Gate>,
    refuse: bool,
    log: mpsc::Sender<Did>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.log.send(Did::Dropped(thread_name()));
    }
}

impl MediaStream for Reader {
    fn duration_ms(&self) -> u64 {
        self.frames * 100
    }
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        if self.refuse {
            return Err(DecoderError::Seek("refused".into()));
        }
        self.next = position_ms / 100;
        let _ = self.log.send(Did::Sought(thread_name(), position_ms));
        Ok(())
    }
}

impl VideoStream for Reader {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        if self.block_at == Some(self.next) {
            self.gate.wait();
        }
        if self.next >= self.frames {
            return Ok(None);
        }
        let k = self.next;
        self.next += 1;
        let _ = self.log.send(Did::Read(thread_name(), k));
        Ok(Some(DecodedVideoFrame {
            data: vec![k as u8; 12],
            width: 4,
            height: 2,
            stride: 4,
            timestamp_ms: k * 100,
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

/// Spawn a threaded reader of `frames` frames on a thread named `video-t`.
fn spawn(
    frames: u64,
    block_at: Option<u64>,
    gate: &Arc<Gate>,
    refuse: bool,
) -> (ThreadedVideo, mpsc::Receiver<Did>) {
    let (log, did) = mpsc::channel();
    let gate = Arc::clone(gate);
    let (video, ()) = ThreadedVideo::spawn(
        |body| {
            std::thread::Builder::new()
                .name("video-t".into())
                .spawn(body)
                .map(drop)
        },
        move || {
            let _ = log.send(Did::Opened(thread_name()));
            let reader = Reader {
                frames,
                next: 0,
                block_at,
                gate,
                refuse,
                log,
            };
            Ok((Box::new(reader) as Box<dyn VideoStream>, ()))
        },
    )
    .expect("the reader opens");
    (video, did)
}

/// The frame index a frame carries.
fn index(frame: Option<DecodedVideoFrame>) -> u64 {
    u64::from(frame.expect("a frame").data[0])
}

/// The next log entry within the bound.
fn next(did: &mpsc::Receiver<Did>) -> Did {
    did.recv_timeout(BOUND).expect("the reader thread logs on")
}

#[test]
fn frames_come_in_order_and_the_reader_lives_on_its_own_thread() {
    let gate = Gate::closed();
    let (mut video, did) = spawn(3, None, &gate, false);
    assert_eq!((video.width(), video.height()), (4, 2));
    assert_eq!(video.frame_rate(), (10, 1));
    assert_eq!(video.duration_ms(), 300);
    for k in 0..3 {
        assert_eq!(index(video.next_frame().unwrap()), k);
    }
    assert!(video.next_frame().unwrap().is_none(), "end-of-stream");
    drop(video);
    let log: Vec<Did> = std::iter::from_fn(|| did.recv_timeout(BOUND).ok()).collect();
    assert_eq!(log[0], Did::Opened("video-t".into()));
    assert_eq!(log.last(), Some(&Did::Dropped("video-t".into())));
    assert!(
        log.iter().all(|d| match d {
            Did::Opened(t) | Did::Read(t, _) | Did::Sought(t, _) | Did::Dropped(t) =>
                t == "video-t",
        }),
        "{log:?}"
    );
}

#[test]
fn an_open_error_comes_back_and_a_spawn_failure_too() {
    let opened = ThreadedVideo::spawn(
        |body| std::thread::Builder::new().spawn(body).map(drop),
        || -> Result<(Box<dyn VideoStream>, ()), DecoderError> {
            Err(DecoderError::Io("no such file".into()))
        },
    );
    match opened {
        Err(DecoderError::Io(text)) => assert_eq!(text, "no such file"),
        other => panic!("the open error: {other:?}"),
    }
    let refused = ThreadedVideo::spawn(
        |_body| Err(std::io::Error::other("no thread")),
        || -> Result<(Box<dyn VideoStream>, u8), DecoderError> { unreachable!("never opened") },
    );
    match refused {
        Err(DecoderError::Io(text)) => assert!(text.contains("no thread"), "{text}"),
        other => panic!("the spawn error: {other:?}"),
    }
}

#[test]
fn a_blocked_read_is_not_ready_within_the_wait_then_comes() {
    let gate = Gate::closed();
    let (mut video, _did) = spawn(4, Some(1), &gate, false);
    let first = video.poll_frame(BOUND).unwrap();
    assert!(matches!(first, FramePoll::Ready(Some(ref f)) if f.data[0] == 0));
    for _ in 0..3 {
        let polled = video.poll_frame(Duration::from_millis(20)).unwrap();
        assert!(matches!(polled, FramePoll::NotReady), "{polled:?}");
    }
    gate.open();
    let back = video.poll_frame(BOUND).unwrap();
    assert!(matches!(back, FramePoll::Ready(Some(ref f)) if f.data[0] == 1));
    assert_eq!(index(video.next_frame().unwrap()), 2);
}

#[test]
fn a_seek_drops_what_was_read_before_it() {
    let gate = Gate::closed();
    let (mut video, _did) = spawn(10, None, &gate, false);
    assert_eq!(index(video.next_frame().unwrap()), 0);
    // The thread has frame 1 in hand, waiting to hand it over.
    video.seek(500).unwrap();
    assert_eq!(index(video.next_frame().unwrap()), 5);
    assert_eq!(index(video.next_frame().unwrap()), 6);
}

#[test]
fn a_refused_seek_keeps_the_stream_as_it_was() {
    let gate = Gate::closed();
    let (mut video, _did) = spawn(5, None, &gate, true);
    assert_eq!(index(video.next_frame().unwrap()), 0);
    assert!(matches!(video.seek(300), Err(DecoderError::Seek(_))));
    for k in 1..5 {
        assert_eq!(index(video.next_frame().unwrap()), k, "no frame lost");
    }
}

#[test]
fn a_seek_after_the_end_reads_again_from_the_new_place() {
    let gate = Gate::closed();
    let (mut video, _did) = spawn(2, None, &gate, false);
    assert_eq!(index(video.next_frame().unwrap()), 0);
    assert_eq!(index(video.next_frame().unwrap()), 1);
    assert!(video.next_frame().unwrap().is_none());
    video.seek(100).unwrap();
    assert_eq!(index(video.next_frame().unwrap()), 1);
}

#[test]
fn a_seek_during_a_blocked_read_waits_for_it_and_drops_the_late_frame() {
    let gate = Gate::closed();
    let (mut video, did) = spawn(10, Some(1), &gate, false);
    assert_eq!(index(video.next_frame().unwrap()), 0);
    let opener = Arc::clone(&gate);
    let (sought_tx, sought) = mpsc::channel();
    let seeker = std::thread::spawn(move || {
        let done = video.seek(700);
        let _ = sought_tx.send(());
        (video, done)
    });
    // Safe direction: the seek cannot finish while the read is blocked.
    assert!(sought.recv_timeout(Duration::from_millis(200)).is_err());
    opener.open();
    sought
        .recv_timeout(BOUND)
        .expect("the seek ends once the read returns");
    let (mut video, done) = seeker.join().unwrap();
    done.unwrap();
    assert_eq!(index(video.next_frame().unwrap()), 7, "frame 1 was dropped");
    let log: Vec<Did> = std::iter::from_fn(|| did.try_recv().ok()).collect();
    assert!(log.contains(&Did::Sought("video-t".into(), 700)), "{log:?}");
}

#[test]
fn dropping_never_waits_for_a_blocked_read() {
    let gate = Gate::closed();
    let (mut video, did) = spawn(5, Some(1), &gate, false);
    assert_eq!(index(video.next_frame().unwrap()), 0);
    let (dropped_tx, dropped) = mpsc::channel();
    std::thread::spawn(move || {
        drop(video);
        let _ = dropped_tx.send(());
    });
    dropped
        .recv_timeout(BOUND)
        .expect("the drop returns while the read still blocks");
    // The reader is dropped on its own thread once the read returns.
    gate.open();
    loop {
        match next(&did) {
            Did::Dropped(thread) => {
                assert_eq!(thread, "video-t");
                break;
            }
            _ => continue,
        }
    }
}

#[test]
fn a_gone_reader_thread_is_an_error_not_a_hang() {
    struct Panics;
    impl MediaStream for Panics {
        fn duration_ms(&self) -> u64 {
            0
        }
        fn seek(&mut self, _: u64) -> Result<(), DecoderError> {
            Ok(())
        }
    }
    impl VideoStream for Panics {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
            panic!("the reader dies (#318 test)")
        }
        fn width(&self) -> u32 {
            4
        }
        fn height(&self) -> u32 {
            2
        }
        fn frame_rate(&self) -> (u32, u32) {
            (30, 1)
        }
    }
    let (mut video, ()) = ThreadedVideo::spawn(
        |body| std::thread::Builder::new().spawn(body).map(drop),
        || Ok((Box::new(Panics) as Box<dyn VideoStream>, ())),
    )
    .unwrap();
    assert!(matches!(video.next_frame(), Err(DecoderError::Decode(_))));
    assert!(matches!(
        video.poll_frame(BOUND),
        Err(DecoderError::Decode(_))
    ));
    assert!(matches!(video.seek(0), Err(DecoderError::Decode(_))));
}
