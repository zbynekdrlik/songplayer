//! Bounded look-ahead frame queue for the #147 producer/consumer paced pipeline.
//!
//! Box test 4 (2026-09-15) proved a ONE-frame synchronous look-ahead cannot hold
//! the grid on the shared box: the emit thread decoded inline inside
//! `Pacer::prepare`, and 1440p decode p99 (93–111 ms) exceeds the 41.7 ms slot
//! while the #162 stems child is resident, so 85 % of emits land late. The fix is
//! the standard producer/consumer split: a dedicated decode thread fills this
//! BOUNDED queue (≥ 8 frames ≈ 330 ms of look-ahead, > 3× the observed p99) and
//! the emit thread only pops at the boundary.
//!
//! This module is the PURE, cross-platform DECISION layer — bound enforcement,
//! end-of-stream drain, and the seek-epoch guard — so every one of those
//! decisions is Linux-testable + mutation-scored. The Windows producer thread and
//! the `Mutex`/`Condvar` wrapper that drive it live in `pipeline_paced.rs`
//! (`SharedQueue`, below).

use std::collections::VecDeque;

/// The outcome of a producer [`PacedQueue::push`].
#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome<T> {
    /// Enqueued; `depth` is the queue depth after the push.
    Accepted { depth: usize },
    /// The queue is at its bound — the producer must wait for the consumer to pop
    /// (backpressure). The frame is handed back so the caller can retry it.
    Full(T),
    /// The frame was decoded BEFORE a seek (its epoch is stale) — dropped, never
    /// shown. The frame is handed back so the caller can discard it.
    Stale(T),
}

/// A bounded FIFO of decoded frames shared between the decode producer and the
/// emit consumer. Pure — no threading, no I/O. Generic over the frame payload so
/// tests drive it with a lightweight stand-in.
pub struct PacedQueue<T> {
    buf: VecDeque<T>,
    bound: usize,
    /// Seek generation. A frame is enqueued only if it was decoded under the
    /// CURRENT epoch; [`begin_seek`](Self::begin_seek) bumps it so any in-flight
    /// pre-seek frame is rejected as [`PushOutcome::Stale`].
    epoch: u64,
    /// The producer reached end-of-stream — no more frames will arrive (until a
    /// seek clears it).
    eos: bool,
}

impl<T> PacedQueue<T> {
    /// Build an empty queue bounded to `bound` frames of look-ahead.
    pub fn new(bound: usize) -> Self {
        assert!(bound >= 1, "queue bound must be >= 1");
        Self {
            buf: VecDeque::with_capacity(bound),
            bound,
            epoch: 0,
            eos: false,
        }
    }

    /// Current queue depth (frames buffered).
    pub fn depth(&self) -> usize {
        self.buf.len()
    }

    /// The queue is at its bound — the producer must wait.
    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.bound
    }

    /// The current seek generation.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The producer has reached end-of-stream.
    pub fn eos(&self) -> bool {
        self.eos
    }

    /// Producer: enqueue `frame` decoded under `frame_epoch`. Dropped (returned)
    /// if its epoch is stale (a seek raced ahead of it) or the queue is at its
    /// bound; otherwise appended and NEVER exceeds `bound`.
    pub fn push(&mut self, frame: T, frame_epoch: u64) -> PushOutcome<T> {
        if frame_epoch != self.epoch {
            return PushOutcome::Stale(frame);
        }
        if self.is_full() {
            return PushOutcome::Full(frame);
        }
        self.buf.push_back(frame);
        PushOutcome::Accepted {
            depth: self.buf.len(),
        }
    }

    /// Consumer: dequeue the oldest frame, or `None` when empty (a producer stall
    /// → the pacer repeats its last frame; never a hole).
    pub fn pop(&mut self) -> Option<T> {
        self.buf.pop_front()
    }

    /// Producer: end-of-stream reached — no more frames will arrive.
    pub fn mark_eos(&mut self) {
        self.eos = true;
    }

    /// Consumer: the producer finished AND every queued frame has been popped —
    /// the song is done (EOS DRAINS the queue, it never truncates it).
    pub fn is_drained(&self) -> bool {
        self.eos && self.buf.is_empty()
    }

    /// Consumer: the song's first frame is buffered, or the producer already
    /// reached end-of-stream (an empty file) — the paced pre-roll may anchor
    /// the song's grid now (#147 song-start hole). Until then the pipeline keeps
    /// filling boundaries with the standby pair.
    pub fn is_primed(&self) -> bool {
        self.eos || !self.buf.is_empty()
    }

    /// Consumer: a seek happened — drop every buffered (now-stale) frame, clear
    /// end-of-stream, and bump the epoch so an in-flight producer push decoded
    /// under the old epoch is rejected as [`PushOutcome::Stale`]. Returns the new
    /// epoch for the producer to adopt after it seeks.
    pub fn begin_seek(&mut self) -> u64 {
        self.buf.clear();
        self.eos = false;
        self.epoch += 1;
        self.epoch
    }
}

use std::sync::{Condvar, Mutex};

/// What a blocking [`SharedQueue::producer_push`] / [`SharedQueue::wait_after_eos`]
/// tells the decode producer to do next.
#[derive(Debug, PartialEq, Eq)]
pub enum ProducerAction {
    /// The frame was enqueued (or dropped as stale) — keep decoding.
    Continue,
    /// A seek is pending: seek the decoder to `position_ms`, adopt `epoch`, and
    /// DISCARD the frame that was being pushed (it was decoded pre-seek).
    Seek { position_ms: u64, epoch: u64 },
    /// The consumer asked the producer to stop (song end / shutdown) — exit the
    /// thread so the decoder drops on its own STA thread.
    Stop,
}

struct QueueState<T> {
    queue: PacedQueue<T>,
    stop: bool,
    /// Set by the consumer on a Seek; consumed by the producer. Carries the seek
    /// target; the epoch is already bumped in `queue` by `begin_seek`.
    pending_seek: Option<u64>,
}

/// Thread-safe wrapper around [`PacedQueue`] driving the #147 decode producer and
/// the emit consumer: a `Mutex` guarding the pure queue + a `not_full` `Condvar`
/// for producer backpressure. The DECISIONS (bound, drain, epoch) all live in the
/// pure `PacedQueue`; this is the (Windows-consumed, box-verified) `Mutex`/
/// `Condvar` plumbing, so its methods are `mutants::skip` glue.
pub struct SharedQueue<T> {
    inner: Mutex<QueueState<T>>,
    not_full: Condvar,
}

impl<T> SharedQueue<T> {
    /// Build a shared queue bounded to `bound` frames of look-ahead.
    #[cfg_attr(test, mutants::skip)]
    pub fn new(bound: usize) -> Self {
        Self {
            inner: Mutex::new(QueueState {
                queue: PacedQueue::new(bound),
                stop: false,
                pending_seek: None,
            }),
            not_full: Condvar::new(),
        }
    }

    /// Producer: push `frame` (decoded under `frame_epoch`), BLOCKING while the
    /// queue is full until the consumer pops, a seek arrives, or stop is set.
    /// Returns the [`ProducerAction`] to take next. A poisoned mutex maps to
    /// `Stop` (the consumer is gone).
    #[cfg_attr(test, mutants::skip)]
    pub fn producer_push(&self, frame: T, frame_epoch: u64) -> ProducerAction {
        let mut frame = Some(frame);
        let mut st = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return ProducerAction::Stop,
        };
        loop {
            if st.stop {
                return ProducerAction::Stop;
            }
            if let Some(position_ms) = st.pending_seek.take() {
                // The frame in hand was decoded before this seek — discard it.
                return ProducerAction::Seek {
                    position_ms,
                    epoch: st.queue.epoch(),
                };
            }
            match st
                .queue
                .push(frame.take().expect("frame present"), frame_epoch)
            {
                PushOutcome::Accepted { .. } | PushOutcome::Stale(_) => {
                    // Stale = a seek raced ahead: the next loop / call surfaces it;
                    // either way this frame is done.
                    return ProducerAction::Continue;
                }
                PushOutcome::Full(f) => {
                    // Backpressure: put the frame back and wait for room / seek /
                    // stop, then retry.
                    frame = Some(f);
                    st = match self.not_full.wait(st) {
                        Ok(g) => g,
                        Err(_) => return ProducerAction::Stop,
                    };
                }
            }
        }
    }

    /// Consumer: pop the oldest frame without blocking, or `None` when empty
    /// (starvation → the pacer repeats its last frame). Signals `not_full` so a
    /// backpressured producer can push. Poisoned mutex → `None`.
    #[cfg_attr(test, mutants::skip)]
    pub fn consumer_pop(&self) -> Option<T> {
        let mut st = self.inner.lock().ok()?;
        let f = st.queue.pop();
        if f.is_some() {
            self.not_full.notify_one();
        }
        f
    }

    /// Producer: mark end-of-stream (no more frames until a seek). Poison → no-op.
    #[cfg_attr(test, mutants::skip)]
    pub fn producer_eos(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.queue.mark_eos();
        }
    }

    /// Producer: after EOS, BLOCK until the consumer requests a seek (scrub after
    /// end) or stop. Returns `Seek{..}` or `Stop`; never `Continue`. Poison → Stop.
    #[cfg_attr(test, mutants::skip)]
    pub fn wait_after_eos(&self) -> ProducerAction {
        let mut st = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return ProducerAction::Stop,
        };
        loop {
            if st.stop {
                return ProducerAction::Stop;
            }
            if let Some(position_ms) = st.pending_seek.take() {
                return ProducerAction::Seek {
                    position_ms,
                    epoch: st.queue.epoch(),
                };
            }
            st = match self.not_full.wait(st) {
                Ok(g) => g,
                Err(_) => return ProducerAction::Stop,
            };
        }
    }

    /// Consumer: a Seek command arrived — flush the buffered (stale) frames, bump
    /// the epoch, and hand the producer the new target. Wakes a backpressured /
    /// post-EOS producer. Poison → no-op.
    #[cfg_attr(test, mutants::skip)]
    pub fn request_seek(&self, position_ms: u64) {
        if let Ok(mut st) = self.inner.lock() {
            st.queue.begin_seek();
            st.pending_seek = Some(position_ms);
            self.not_full.notify_all();
        }
    }

    /// Consumer: tell the producer to stop (song end / shutdown) and wake it.
    /// Poison → no-op (the producer already saw the disconnect).
    #[cfg_attr(test, mutants::skip)]
    pub fn stop(&self) {
        if let Ok(mut st) = self.inner.lock() {
            st.stop = true;
            self.not_full.notify_all();
        }
    }

    /// Consumer: the producer finished AND the buffer is empty — the song is done.
    /// Poison → true (nothing more will arrive; end cleanly).
    #[cfg_attr(test, mutants::skip)]
    pub fn is_drained(&self) -> bool {
        match self.inner.lock() {
            Ok(st) => st.queue.is_drained(),
            Err(_) => true,
        }
    }

    /// Consumer: the first frame (or end-of-stream) is available — see
    /// [`PacedQueue::is_primed`]. Does NOT pop. Poison → true (nothing more will
    /// arrive; let the pre-roll end and the song end cleanly).
    #[cfg_attr(test, mutants::skip)]
    pub fn is_primed(&self) -> bool {
        match self.inner.lock() {
            Ok(st) => st.queue.is_primed(),
            Err(_) => true,
        }
    }
}

#[cfg(test)]
#[path = "pacer_queue_tests.rs"]
mod tests;
