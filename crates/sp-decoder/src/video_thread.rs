//! The video reader on its own thread (#318).
//!
//! The paced decode producer used to read the video AND, for each picture,
//! the audio up to the picture's time plus the 250 ms lead, on one thread
//! (`SplitSyncedDecoder::next_synced`). When Media Foundation's D3D11
//! decoder blocked (SNV, 13.11.2026: a GPU display reconfiguration held a
//! read 0.5–0.75 s), no audio was read either, and the song's audio grid ran
//! dry (`underruns_mid`). Now the reader lives on a thread of its own and
//! hands each picture over through a rendezvous channel, so the producer can
//! ask "is the next picture here yet?" ([`VideoStream::poll_frame`]) and go
//! on serving audio on the held picture while it is not
//! (`SplitSyncedDecoder::next_late`).
//!
//! - **One thread owns the reader.** It opens it ([`ThreadedVideo::spawn`]'s
//!   `open`, run on the new thread), reads, seeks and drops it, as Media
//!   Foundation's apartment rule asks (`genlock.md`: open / read / seek / drop
//!   on ONE thread). The caller starts the thread ([`spawn`](ThreadedVideo::spawn)'s
//!   `spawn`), so sp-server schedules it like every decode thread.
//! - **A rendezvous hand-off.** The thread decodes one picture, then waits for
//!   the producer to take it: at most one picture read ahead (one more ring
//!   slot of a GPU-resident pipeline than before, `video-decode.md`).
//! - **A seek** goes to the thread and waits for its answer, so a refused
//!   seek is still reported (#217). While the thread waits to hand a picture
//!   over it serves the seek at once; while it is inside a read (a stall) the
//!   seek waits for that read, at most [`SEEK_WAIT`]: then it gives up, the
//!   thread never applies it (a three-state claim, [`SeekClaim`]) and the
//!   seek is REFUSED (`DecoderError::Seek`, the #217 path: the song plays on
//!   from where it was), so a read that never returns can never hang the
//!   producer (review round 2). A picture read before an accepted seek is
//!   dropped on the thread (the hand-off is a rendezvous: a read is either
//!   taken before the seek is sent, or still in the thread's hand); a
//!   refused seek keeps the stream as it was.
//! - **Stop** = drop: the channels close, the thread ends once its current
//!   read returns and drops the reader there. Dropping never waits for a
//!   blocked read (the emit thread joins the producer before the next song),
//!   so a reader stuck in a read OUTLIVES its producer: the pipeline's next
//!   song can open its own reader (and its file) while the old one still
//!   waits, holding its file handle and, GPU-resident, one ring slot.
//! - **A stuck reader is reported** (#319): the drop hands the thread's
//!   "done" signal to a watcher thread; a reader thread still alive
//!   [`DETACH_WARN_AFTER`] after its owner dropped it is one WARN naming
//!   the file and a count in [`detached_readers`] (`video_thread_watch.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Select, Sender, TryRecvError};
use tracing::warn;

#[path = "video_thread_watch.rs"]
mod watch;

pub use watch::{DETACH_WARN_AFTER, DetachedCounts, DetachedReaders, detached_readers};

use crate::error::DecoderError;
use crate::stream::{FramePoll, MediaStream, VideoStream};
use crate::types::DecodedVideoFrame;

/// What a reader read.
type Read = Result<Option<DecodedVideoFrame>, DecoderError>;

/// How long a seek waits for the reader thread to take it (#318 review
/// round 2): a read blocked longer than this refuses the seek. Above the
/// 0.5-0.75 s D3D11 stalls measured on SNV, far under the song's 15 s bound.
pub const SEEK_WAIT: Duration = Duration::from_secs(2);

/// Who settled a seek first (#318 review round 2): the thread taking it, or
/// the caller giving up. One compare-and-swap each, so exactly one wins.
#[derive(Debug, Default)]
struct SeekClaim(AtomicU8);

impl SeekClaim {
    const PENDING: u8 = 0;
    const TAKEN: u8 = 1;
    const ABANDONED: u8 = 2;

    /// The thread takes the seek; `false` = the caller gave it up first.
    fn take(&self) -> bool {
        self.settle(Self::TAKEN)
    }

    /// The caller gives the seek up; `false` = the thread took it first.
    fn abandon(&self) -> bool {
        self.settle(Self::ABANDONED)
    }

    fn settle(&self, to: u8) -> bool {
        self.0
            .compare_exchange(Self::PENDING, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// A seek for the reader thread, answered on `reply` unless abandoned.
struct Seek {
    position_ms: u64,
    claim: Arc<SeekClaim>,
    reply: Sender<Result<(), DecoderError>>,
}

/// What the reader says of its stream, read once on its thread at open.
#[derive(Debug, Clone, Copy)]
struct Facts {
    duration_ms: u64,
    width: u32,
    height: u32,
    frame_rate: (u32, u32),
}

impl Facts {
    fn of(reader: &dyn VideoStream) -> Self {
        Self {
            duration_ms: reader.duration_ms(),
            width: reader.width(),
            height: reader.height(),
            frame_rate: reader.frame_rate(),
        }
    }
}

/// The error of a reader thread that is gone (it panicked).
fn gone() -> DecoderError {
    DecoderError::Decode("the video reader thread is gone".into())
}

/// A [`VideoStream`] whose reader runs on its own thread (module doc).
pub struct ThreadedVideo {
    reads: Receiver<Read>,
    seeks: Sender<Seek>,
    facts: Facts,
    /// Disconnects when the reader thread ends (#319: its drop watches it).
    done: Receiver<()>,
    /// The file the reader reads, for the stuck reader's WARN.
    file: String,
    /// How long the drop's watcher waits for the thread's end.
    watch_after: Duration,
    /// Where a stuck reader is counted.
    counts: &'static DetachedReaders,
}

impl std::fmt::Debug for ThreadedVideo {
    #[cfg_attr(test, mutants::skip)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadedVideo")
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

impl ThreadedVideo {
    /// Start the reader thread with `spawn` (handed the thread's body) and
    /// open the reader of `file` (its name, for the log) on it with `open`,
    /// which also returns what the caller wants of the reader at open (`T`:
    /// Media Foundation's codec name). Returns once the reader opened, or its
    /// open error.
    pub fn spawn<T, S, O>(
        spawn: S,
        file: impl Into<String>,
        open: O,
    ) -> Result<(Self, T), DecoderError>
    where
        T: Send + 'static,
        S: FnOnce(Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()>,
        O: FnOnce() -> Result<(Box<dyn VideoStream>, T), DecoderError> + Send + 'static,
    {
        let (opened_tx, opened_rx) = crossbeam_channel::bounded(1);
        let (reads_tx, reads_rx) = crossbeam_channel::bounded(0);
        let (seeks_tx, seeks_rx) = crossbeam_channel::unbounded();
        let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(0);
        spawn(Box::new(move || {
            // #319: held for the thread's whole life, so its end (a return
            // or a panic) disconnects `done`.
            let _alive = done_tx;
            let (reader, extra) = match open() {
                Ok(opened) => opened,
                Err(e) => {
                    let _ = opened_tx.send(Err(e));
                    return;
                }
            };
            let facts = Facts::of(&*reader);
            if opened_tx.send(Ok((facts, extra))).is_ok() {
                serve(reader, &reads_tx, &seeks_rx);
            }
        }))
        .map_err(|e| DecoderError::Io(format!("the video reader thread did not start: {e}")))?;
        let (facts, extra) = opened_rx.recv().map_err(|_| gone())??;
        let video = Self {
            reads: reads_rx,
            seeks: seeks_tx,
            facts,
            done: done_rx,
            file: file.into(),
            watch_after: DETACH_WARN_AFTER,
            counts: detached_readers(),
        };
        Ok((video, extra))
    }

    /// Watch this reader's thread for `after` at its drop and count a stuck
    /// one in `counts` (#319; tests only).
    #[doc(hidden)]
    pub fn set_detach_watch(&mut self, after: Duration, counts: &'static DetachedReaders) {
        self.watch_after = after;
        self.counts = counts;
    }
}

impl Drop for ThreadedVideo {
    /// Hand the reader thread's end to a watcher (#319): the drop itself
    /// never waits (#318).
    fn drop(&mut self) {
        let done = std::mem::replace(&mut self.done, crossbeam_channel::never());
        let file = std::mem::take(&mut self.file);
        let (after, counts) = (self.watch_after, self.counts);
        let watcher = std::thread::Builder::new()
            .name("video-reader-watch".into())
            .spawn(move || watch::watch(&done, after, &file, counts));
        if let Err(e) = watcher {
            warn!(
                error = %e,
                "video reader: the watcher of a dropped reader did not start (#319)"
            );
        }
    }
}

/// The reader thread's loop: read a picture, hand it over, serve the seeks;
/// at end-of-stream (or after an error) read nothing until a seek. Ends when
/// the other side is gone.
fn serve(mut reader: Box<dyn VideoStream>, reads: &Sender<Read>, seeks: &Receiver<Seek>) {
    let mut ended = false;
    loop {
        if ended {
            let Ok(seek) = seeks.recv() else {
                return;
            };
            ended = !apply(&mut *reader, seek);
            continue;
        }
        match seeks.try_recv() {
            Ok(seek) => {
                apply(&mut *reader, seek);
                continue;
            }
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }
        let read = reader.next_frame();
        ended = !matches!(read, Ok(Some(_)));
        match hand_over(&mut *reader, read, reads, seeks) {
            HandOver::Taken => {}
            // The end (or error) read belonged to the old generation: the
            // reader reads again from its new place.
            HandOver::Sought => ended = false,
            HandOver::Gone => return,
        }
    }
}

/// How a hand-over ended.
enum HandOver {
    /// The other side took the read.
    Taken,
    /// A seek moved the reader first: the read is dropped (stale; a GPU
    /// picture frees its ring slot with it).
    Sought,
    /// The other side is gone.
    Gone,
}

/// Wait until the other side takes `read`, serving every seek meanwhile. A
/// seek that moves the reader drops `read`; a refused one keeps it.
fn hand_over(
    reader: &mut dyn VideoStream,
    read: Read,
    reads: &Sender<Read>,
    seeks: &Receiver<Seek>,
) -> HandOver {
    loop {
        let mut select = Select::new();
        let send = select.send(reads);
        select.recv(seeks);
        let oper = select.select();
        if oper.index() == send {
            return match oper.send(reads, read) {
                Ok(()) => HandOver::Taken,
                Err(_) => HandOver::Gone,
            };
        }
        let Ok(seek) = oper.recv(seeks) else {
            return HandOver::Gone;
        };
        if apply(reader, seek) {
            return HandOver::Sought;
        }
        // Refused: nothing moved, the read stays the next one.
    }
}

/// Seek the reader and answer; returns whether it moved.
fn apply(reader: &mut dyn VideoStream, seek: Seek) -> bool {
    if !seek.claim.take() {
        // Given up by the caller (it reported the seek refused): nothing moves.
        return false;
    }
    let done = reader.seek(seek.position_ms);
    let moved = done.is_ok();
    let _ = seek.reply.send(done);
    moved
}

impl MediaStream for ThreadedVideo {
    fn duration_ms(&self) -> u64 {
        self.facts.duration_ms
    }

    /// Seek on the reader thread and wait for its answer (module doc).
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        let (reply, answer) = crossbeam_channel::bounded(1);
        let claim = Arc::new(SeekClaim::default());
        let seek = Seek {
            position_ms,
            claim: Arc::clone(&claim),
            reply,
        };
        self.seeks.send(seek).map_err(|_| gone())?;
        match answer.recv_timeout(SEEK_WAIT) {
            Ok(done) => done,
            Err(RecvTimeoutError::Disconnected) => Err(gone()),
            Err(RecvTimeoutError::Timeout) if claim.abandon() => Err(DecoderError::Seek(format!(
                "the video reader is still inside a read after {} ms - the seek is refused (#318)",
                SEEK_WAIT.as_millis()
            ))),
            // The thread took the seek just now: its answer follows.
            Err(RecvTimeoutError::Timeout) => answer.recv().map_err(|_| gone())?,
        }
    }
}

impl VideoStream for ThreadedVideo {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        self.reads.recv().map_err(|_| gone())?
    }

    fn poll_frame(&mut self, wait: Duration) -> Result<FramePoll, DecoderError> {
        let Some(deadline) = Instant::now().checked_add(wait) else {
            return Ok(FramePoll::Ready(self.next_frame()?));
        };
        match self.reads.recv_deadline(deadline) {
            Ok(read) => Ok(FramePoll::Ready(read?)),
            Err(RecvTimeoutError::Timeout) => Ok(FramePoll::NotReady),
            Err(RecvTimeoutError::Disconnected) => Err(gone()),
        }
    }

    fn width(&self) -> u32 {
        self.facts.width
    }

    fn height(&self) -> u32 {
        self.facts.height
    }

    fn frame_rate(&self) -> (u32, u32) {
        self.facts.frame_rate
    }
}

#[cfg(test)]
#[path = "video_thread_tests.rs"]
mod tests;
