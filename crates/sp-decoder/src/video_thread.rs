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
//!   seek waits for that read. Every picture read before an accepted seek is
//!   dropped by its epoch; a refused one keeps the stream as it was.
//! - **Stop** = drop: the channels close, the thread ends once its current
//!   read returns and drops the reader there. Dropping never waits for a
//!   blocked read (the emit thread joins the producer before the next song).

use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Select, Sender, TryRecvError};

use crate::error::DecoderError;
use crate::stream::{FramePoll, MediaStream, VideoStream};
use crate::types::DecodedVideoFrame;

/// What a reader read, under the seek generation it read it in.
struct Read {
    epoch: u64,
    read: Result<Option<DecodedVideoFrame>, DecoderError>,
}

/// A seek for the reader thread, answered on `reply`.
struct Seek {
    position_ms: u64,
    epoch: u64,
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
    /// The seek generation of the pictures this side takes.
    epoch: u64,
    facts: Facts,
}

impl std::fmt::Debug for ThreadedVideo {
    #[cfg_attr(test, mutants::skip)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadedVideo")
            .field("epoch", &self.epoch)
            .field("facts", &self.facts)
            .finish_non_exhaustive()
    }
}

impl ThreadedVideo {
    /// Start the reader thread with `spawn` (handed the thread's body) and
    /// open the reader on it with `open`, which also returns what the caller
    /// wants of the reader at open (`T`: Media Foundation's codec name).
    /// Returns once the reader opened, or its open error.
    pub fn spawn<T, S, O>(spawn: S, open: O) -> Result<(Self, T), DecoderError>
    where
        T: Send + 'static,
        S: FnOnce(Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()>,
        O: FnOnce() -> Result<(Box<dyn VideoStream>, T), DecoderError> + Send + 'static,
    {
        let (opened_tx, opened_rx) = crossbeam_channel::bounded(1);
        let (reads_tx, reads_rx) = crossbeam_channel::bounded(0);
        let (seeks_tx, seeks_rx) = crossbeam_channel::unbounded();
        spawn(Box::new(move || {
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
            epoch: 0,
            facts,
        };
        Ok((video, extra))
    }

    /// A read of the current generation (`None`: one from before the last
    /// seek, dropped; a GPU picture frees its ring slot with it).
    fn current(&self, read: Read) -> Option<Result<Option<DecodedVideoFrame>, DecoderError>> {
        (read.epoch == self.epoch).then_some(read.read)
    }
}

/// The reader thread's loop: read a picture, hand it over, serve the seeks;
/// at end-of-stream (or after an error) read nothing until a seek. Ends when
/// the other side is gone.
fn serve(mut reader: Box<dyn VideoStream>, reads: &Sender<Read>, seeks: &Receiver<Seek>) {
    let mut epoch = 0;
    let mut ended = false;
    loop {
        if ended {
            let Ok(seek) = seeks.recv() else {
                return;
            };
            ended = !apply(&mut *reader, seek, &mut epoch);
            continue;
        }
        match seeks.try_recv() {
            Ok(seek) => {
                apply(&mut *reader, seek, &mut epoch);
                continue;
            }
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }
        let read = reader.next_frame();
        ended = !matches!(read, Ok(Some(_)));
        let read = Read { epoch, read };
        match hand_over(&mut *reader, read, reads, seeks, &mut epoch) {
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
    /// A seek moved the reader first: the read is dropped (stale).
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
    epoch: &mut u64,
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
        if apply(reader, seek, epoch) {
            return HandOver::Sought;
        }
        // Refused: nothing moved, the read stays the next one.
    }
}

/// Seek the reader; returns whether it moved (the new generation adopted).
fn apply(reader: &mut dyn VideoStream, seek: Seek, epoch: &mut u64) -> bool {
    let done = reader.seek(seek.position_ms);
    let moved = done.is_ok();
    if moved {
        *epoch = seek.epoch;
    }
    let _ = seek.reply.send(done);
    moved
}

impl MediaStream for ThreadedVideo {
    fn duration_ms(&self) -> u64 {
        self.facts.duration_ms
    }

    /// Seek on the reader thread and wait for its answer (module doc).
    fn seek(&mut self, position_ms: u64) -> Result<(), DecoderError> {
        let epoch = self.epoch + 1;
        let (reply, answer) = crossbeam_channel::bounded(1);
        self.seeks
            .send(Seek {
                position_ms,
                epoch,
                reply,
            })
            .map_err(|_| gone())?;
        answer.recv().map_err(|_| gone())??;
        self.epoch = epoch;
        Ok(())
    }
}

impl VideoStream for ThreadedVideo {
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>, DecoderError> {
        loop {
            let read = self.reads.recv().map_err(|_| gone())?;
            if let Some(read) = self.current(read) {
                return read;
            }
        }
    }

    fn poll_frame(&mut self, wait: Duration) -> Result<FramePoll, DecoderError> {
        let Some(deadline) = Instant::now().checked_add(wait) else {
            return Ok(FramePoll::Ready(self.next_frame()?));
        };
        loop {
            match self.reads.recv_deadline(deadline) {
                Ok(read) => {
                    if let Some(read) = self.current(read) {
                        return Ok(FramePoll::Ready(read?));
                    }
                }
                Err(RecvTimeoutError::Timeout) => return Ok(FramePoll::NotReady),
                Err(RecvTimeoutError::Disconnected) => return Err(gone()),
            }
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
