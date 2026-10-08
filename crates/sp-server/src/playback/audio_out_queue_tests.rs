//! #233: the ASIO output's block queue — drop-oldest over its bound (the
//! newest kept, in order), counted; a stop drains, a discard drops; a stopped
//! queue takes nothing; a waiting take wakes on a push and never waits while
//! a block is queued.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::playback::audio_out_block::ProgramBlock;

fn due(t: &Take) -> Option<i64> {
    match t {
        Take::Block(b) => Some(b.due_100ns),
        _ => None,
    }
}

#[test]
fn over_its_bound_the_oldest_block_goes_and_is_counted() {
    let q = BlockQueue::new(2);
    assert_eq!(q.bound(), 2);
    assert_eq!(q.push(ProgramBlock::silence(1)), None);
    assert_eq!(q.push(ProgramBlock::silence(2)), None);
    assert_eq!(q.queued(), 2);
    assert_eq!(q.push(ProgramBlock::silence(3)), Some(1), "the first drop");
    assert_eq!(q.push(ProgramBlock::silence(4)), Some(2));
    assert_eq!((q.queued(), q.dropped()), (2, 2));
    assert_eq!(
        due(&q.take_timeout(Duration::ZERO)),
        Some(3),
        "oldest kept first"
    );
    assert_eq!(due(&q.take_timeout(Duration::ZERO)), Some(4));
    assert_eq!(q.take_timeout(Duration::ZERO), Take::Idle);
}

#[test]
fn a_stop_drains_then_ends_and_takes_nothing_more() {
    let q = BlockQueue::new(4);
    q.push(ProgramBlock::silence(1));
    q.stop();
    assert_eq!(q.push(ProgramBlock::silence(2)), None);
    assert_eq!(q.queued(), 1, "nothing taken after the stop");
    assert_eq!(due(&q.take_timeout(Duration::ZERO)), Some(1));
    assert_eq!(q.take_timeout(Duration::ZERO), Take::Stopped);
}

#[test]
fn a_discard_drops_what_is_queued_and_takes_nothing_more() {
    let q = BlockQueue::new(4);
    q.push(ProgramBlock::silence(1));
    q.push(ProgramBlock::silence(2));
    q.discard();
    assert_eq!(q.queued(), 0);
    q.push(ProgramBlock::silence(3));
    assert_eq!(q.queued(), 0);
    assert_eq!(q.take_timeout(Duration::ZERO), Take::Stopped);
    assert_eq!(q.dropped(), 0, "a discard is no overflow");
}

#[test]
fn a_waiting_take_wakes_on_a_push_and_a_queued_block_is_never_waited_on() {
    let q = Arc::new(BlockQueue::new(4));
    let (tx, rx) = mpsc::channel();
    let taker = q.clone();
    std::thread::spawn(move || {
        let _ = tx.send(due(&taker.take_timeout(Duration::from_secs(600))));
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "an empty queue waits"
    );
    q.push(ProgramBlock::silence(7));
    assert_eq!(rx.recv_timeout(Duration::from_secs(20)), Ok(Some(7)));

    q.push(ProgramBlock::silence(8));
    let (tx, rx) = mpsc::channel();
    let taker = q.clone();
    std::thread::spawn(move || {
        let _ = tx.send(due(&taker.take_timeout(Duration::from_secs(600))));
    });
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(20)),
        Ok(Some(8)),
        "a queued block is taken at once"
    );
    let (tx, rx) = mpsc::channel();
    let taker = q.clone();
    std::thread::spawn(move || {
        let _ = tx.send(taker.take_timeout(Duration::from_secs(600)) == Take::Stopped);
    });
    q.stop();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(20)),
        Ok(true),
        "a stop wakes it"
    );
}
