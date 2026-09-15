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

    /// The look-ahead bound (max frames buffered).
    pub fn bound(&self) -> usize {
        self.bound
    }

    /// Current queue depth (frames buffered).
    pub fn depth(&self) -> usize {
        self.buf.len()
    }

    /// The queue holds no frames.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The queue is at its bound — the producer must wait.
    pub fn is_full(&self) -> bool {
        // RED (#147): off-by-one — allows one frame past the bound. GREEN uses
        // `>=`.
        self.buf.len() > self.bound
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
        // RED (#147): no epoch guard, no bound guard — always accepts. GREEN adds
        // the stale-epoch reject and the bound backpressure.
        let _ = frame_epoch;
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
        // RED (#147): ignores the buffered frames — reports "drained" the instant
        // EOS is marked, truncating the tail. GREEN ANDs `self.buf.is_empty()`.
        self.eos
    }

    /// Consumer: a seek happened — drop every buffered (now-stale) frame, clear
    /// end-of-stream, and bump the epoch so an in-flight producer push decoded
    /// under the old epoch is rejected as [`PushOutcome::Stale`]. Returns the new
    /// epoch for the producer to adopt after it seeks.
    pub fn begin_seek(&mut self) -> u64 {
        // RED (#147): does not flush, does not bump the epoch. GREEN clears the
        // buffer + eos and increments the epoch.
        self.epoch
    }
}

// The threading wrapper (`SharedQueue`) that drives this pure queue from the
// Windows decode producer + emit consumer is added alongside the GREEN pipeline
// wiring.

#[cfg(test)]
#[path = "pacer_queue_tests.rs"]
mod tests;
