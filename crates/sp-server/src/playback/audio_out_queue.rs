//! #233: a bounded, never-blocking block queue for an output's thread (the
//! ASIO output's; #210's `VbanOut` keeps its own): over the bound the OLDEST
//! block is dropped and counted. A stopped queue takes no more blocks;
//! `stop` lets the thread drain what is queued (process shutdown), `discard`
//! drops it (a runtime replace or removal, `audio_out_task::apply`).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::playback::audio_out_block::ProgramBlock;

/// What [`BlockQueue::take_timeout`] returned.
#[derive(Debug, PartialEq)]
pub enum Take {
    Block(ProgramBlock),
    /// The wait timed out with nothing queued.
    Idle,
    /// Stopped and drained.
    Stopped,
}

struct Inner {
    blocks: VecDeque<ProgramBlock>,
    stop: bool,
}

/// One output's hand-off queue (every method holds its lock for µs).
pub struct BlockQueue {
    inner: Mutex<Inner>,
    ready: Condvar,
    bound: usize,
    dropped: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl BlockQueue {
    /// An empty queue holding at most `bound` blocks.
    pub fn new(bound: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                blocks: VecDeque::with_capacity(bound + 1),
                stop: false,
            }),
            ready: Condvar::new(),
            bound,
            dropped: AtomicU64::new(0),
        }
    }

    /// Queue a block; never blocks. Returns the blocks dropped so far when
    /// this push dropped the oldest one (for a rate-limited WARN), else
    /// `None`. A stopped queue takes nothing.
    pub fn push(&self, block: ProgramBlock) -> Option<u64> {
        let over = {
            let mut q = lock(&self.inner);
            if q.stop {
                return None;
            }
            q.blocks.push_back(block);
            let over = q.blocks.len() > self.bound;
            if over {
                q.blocks.pop_front();
            }
            over
        };
        self.ready.notify_one();
        over.then(|| self.dropped.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// The next block, waiting at most `wait` for one. Queued blocks are
    /// taken before [`Take::Stopped`].
    pub fn take_timeout(&self, wait: Duration) -> Take {
        let q = lock(&self.inner);
        let (mut q, _) = self
            .ready
            .wait_timeout_while(q, wait, |q| q.blocks.is_empty() && !q.stop)
            .unwrap_or_else(|p| p.into_inner());
        match q.blocks.pop_front() {
            Some(b) => Take::Block(b),
            None if q.stop => Take::Stopped,
            None => Take::Idle,
        }
    }

    /// Drop the queued blocks, keep taking new ones (an ASIO output's open:
    /// what queued while it opened is stale). Returns how many it dropped.
    pub fn clear(&self) -> usize {
        let mut q = lock(&self.inner);
        let n = q.blocks.len();
        q.blocks.clear();
        n
    }

    /// Blocks waiting.
    pub fn queued(&self) -> usize {
        lock(&self.inner).blocks.len()
    }

    /// The bound.
    pub fn bound(&self) -> usize {
        self.bound
    }

    /// Stop once the queued blocks are taken (process shutdown).
    pub fn stop(&self) {
        lock(&self.inner).stop = true;
        self.ready.notify_all();
    }

    /// Stop now: the queued blocks are dropped (a runtime replace or removal).
    pub fn discard(&self) {
        let mut q = lock(&self.inner);
        q.blocks.clear();
        q.stop = true;
        drop(q);
        self.ready.notify_all();
    }

    /// Blocks dropped over the bound.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[path = "audio_out_queue_tests.rs"]
mod tests;
