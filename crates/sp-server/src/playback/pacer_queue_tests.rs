//! Unit tests for the #147 pure bounded look-ahead queue (`PacedQueue`).
//!
//! RED-first: these assert the intended producer/consumer decisions against the
//! stubbed module (wrong bound guard, no epoch guard, EOS ignores the buffer,
//! begin_seek is a no-op), so they FAIL until the GREEN commit fills the logic.
//! Pure — no threads; the `SharedQueue` Mutex/Condvar wrapper is exercised
//! separately.

use super::*;

/// Emit pops the frame due at the boundary from the queue — FIFO order, oldest
/// first (the pacer's `prepare` then applies the presentation rule on top).
#[test]
fn pop_returns_frames_in_fifo_order() {
    let mut q: PacedQueue<u32> = PacedQueue::new(8);
    assert_eq!(q.push(10, 0), PushOutcome::Accepted { depth: 1 });
    assert_eq!(q.push(20, 0), PushOutcome::Accepted { depth: 2 });
    assert_eq!(q.push(30, 0), PushOutcome::Accepted { depth: 3 });
    assert_eq!(q.pop(), Some(10));
    assert_eq!(q.pop(), Some(20));
    assert_eq!(q.pop(), Some(30));
    assert_eq!(q.pop(), None);
}

/// A bound of 1 accepts exactly one frame, then reports full (guards the
/// `bound >= 1` floor in `new`).
#[test]
fn bound_of_one_accepts_one_then_full() {
    let mut q: PacedQueue<u32> = PacedQueue::new(1);
    assert_eq!(q.push(1, 0), PushOutcome::Accepted { depth: 1 });
    assert!(q.is_full());
    match q.push(2, 0) {
        PushOutcome::Full(f) => assert_eq!(f, 2),
        other => panic!("expected Full(2), got {other:?}"),
    }
}

/// The queue never exceeds its bound — the producer's backpressure signal.
#[test]
fn queue_never_exceeds_its_bound() {
    let mut q: PacedQueue<u32> = PacedQueue::new(3);
    assert_eq!(q.push(1, 0), PushOutcome::Accepted { depth: 1 });
    assert_eq!(q.push(2, 0), PushOutcome::Accepted { depth: 2 });
    assert_eq!(q.push(3, 0), PushOutcome::Accepted { depth: 3 });
    assert!(q.is_full(), "at the bound the queue is full");
    // The 4th push is refused (backpressure) and the frame is handed back.
    match q.push(4, 0) {
        PushOutcome::Full(f) => assert_eq!(f, 4, "the refused frame is returned"),
        other => panic!("expected Full(4), got {other:?}"),
    }
    assert_eq!(q.depth(), 3, "depth never grows past the bound");
    // Popping frees room; the next push is accepted.
    assert_eq!(q.pop(), Some(1));
    assert!(!q.is_full());
    assert_eq!(q.push(4, 0), PushOutcome::Accepted { depth: 3 });
}

/// A producer stall past the queue → the consumer pops None → the pacer repeats
/// the last frame. `is_drained` stays false while the stream has not ended.
#[test]
fn empty_pop_is_none_and_not_drained_while_running() {
    let mut q: PacedQueue<u32> = PacedQueue::new(4);
    assert_eq!(q.pop(), None, "empty queue pops None (starvation → repeat)");
    assert!(
        !q.is_drained(),
        "an empty but not-yet-ended stream is NOT drained"
    );
    q.push(7, 0);
    assert_eq!(q.pop(), Some(7));
    assert_eq!(q.pop(), None);
    assert!(
        !q.is_drained(),
        "still running: drained only after mark_eos"
    );
}

/// EOS drains the queue — the consumer keeps popping the buffered frames after
/// the producer marks end-of-stream, and only THEN is the stream drained.
#[test]
fn eos_drains_the_queue_never_truncates() {
    let mut q: PacedQueue<u32> = PacedQueue::new(8);
    q.push(1, 0);
    q.push(2, 0);
    q.mark_eos();
    assert!(q.eos(), "eos flag is set");
    assert!(
        !q.is_drained(),
        "NOT drained while frames remain — the tail must still play"
    );
    assert_eq!(q.pop(), Some(1));
    assert!(!q.is_drained(), "still one frame buffered");
    assert_eq!(q.pop(), Some(2));
    assert!(q.is_drained(), "drained once eos AND the buffer is empty");
    assert_eq!(q.pop(), None);
}

/// A push tagged with a stale epoch (decoded before a seek) is dropped.
#[test]
fn stale_epoch_push_is_dropped() {
    let mut q: PacedQueue<u32> = PacedQueue::new(8);
    assert_eq!(q.epoch(), 0, "fresh queue is at epoch 0");
    q.push(1, 0);
    let new_epoch = q.begin_seek();
    assert_eq!(new_epoch, 1, "begin_seek bumps the epoch");
    assert_eq!(q.epoch(), 1, "epoch() reflects the bump");
    // A frame decoded under the OLD epoch (0) is rejected as stale.
    match q.push(99, 0) {
        PushOutcome::Stale(f) => assert_eq!(f, 99, "the stale frame is returned"),
        other => panic!("expected Stale(99), got {other:?}"),
    }
    assert_eq!(q.depth(), 0, "a stale frame is never enqueued");
}

/// begin_seek flushes the buffered (stale) frames, clears eos, bumps the epoch;
/// a push at the NEW epoch is then accepted.
#[test]
fn begin_seek_flushes_and_a_new_epoch_push_is_accepted() {
    let mut q: PacedQueue<u32> = PacedQueue::new(8);
    q.push(1, 0);
    q.push(2, 0);
    q.mark_eos();
    let e = q.begin_seek();
    assert_eq!(e, 1);
    assert_eq!(q.depth(), 0, "buffered frames are flushed on seek");
    assert!(!q.eos(), "eos is cleared — decoding resumes post-seek");
    // A frame decoded under the new epoch is accepted.
    assert_eq!(q.push(50, 1), PushOutcome::Accepted { depth: 1 });
    // A second seek bumps again.
    assert_eq!(q.begin_seek(), 2);
}

// ---- SharedQueue: the Mutex/Condvar wrapper (integration coverage) --------

/// Blocking backpressure loses no frame and preserves order: a tiny bound forces
/// the producer to block on `not_full` until the consumer pops, and every frame
/// still arrives exactly once, in FIFO order. Deterministic (no timing asserts) —
/// if backpressure were broken, frames would be lost or the bound violated.
#[test]
fn shared_queue_backpressure_preserves_every_frame_in_order() {
    use std::thread;
    let q = SharedQueue::<u32>::new(2);
    let n = 200u32;
    thread::scope(|s| {
        s.spawn(|| {
            for i in 0..n {
                match q.producer_push(i, 0) {
                    ProducerAction::Continue => {}
                    other => panic!("unexpected producer action: {other:?}"),
                }
            }
            q.producer_eos();
        });
        let mut got = Vec::new();
        loop {
            match q.consumer_pop() {
                Some(v) => got.push(v),
                None => {
                    if q.is_drained() {
                        break;
                    }
                    thread::yield_now();
                }
            }
        }
        let expected: Vec<u32> = (0..n).collect();
        assert_eq!(got, expected, "every frame received exactly once, in order");
    });
}

/// A consumer Seek surfaces to the producer as `Seek{position, epoch}` (bumped),
/// discarding the in-hand pre-seek frame; a push under the new epoch then flows.
#[test]
fn shared_queue_request_seek_surfaces_to_producer() {
    let q = SharedQueue::<u32>::new(4);
    q.request_seek(5000);
    match q.producer_push(42, 0) {
        ProducerAction::Seek { position_ms, epoch } => {
            assert_eq!(position_ms, 5000);
            assert_eq!(epoch, 1, "the seek bumped the epoch");
        }
        other => panic!("expected Seek, got {other:?}"),
    }
    // After adopting the new epoch the producer resumes.
    assert_eq!(q.producer_push(7, 1), ProducerAction::Continue);
    assert_eq!(q.consumer_pop(), Some(7));
}

/// A consumer Stop tells the producer to exit.
#[test]
fn shared_queue_stop_tells_producer_to_stop() {
    let q = SharedQueue::<u32>::new(4);
    q.stop();
    assert_eq!(q.producer_push(1, 0), ProducerAction::Stop);
}
