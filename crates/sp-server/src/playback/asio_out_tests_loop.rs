//! #233 (the SoundGrid loop check): the worker's side — an entry with
//! `asio.loop_check` taps each driver start and hands that run's rings to
//! its analyzer thread, started once per output through the seam (a
//! recorder here); the analyzer's status is the output's `loop`, running
//! and waiting; a closed run's taps go with it; an entry without the check
//! taps nothing and reads off.

use std::sync::{Arc, Mutex};

use super::fake::{FakeDevice, dvs};
use super::*;
use crate::playback::asio_loop::{LoopCore, LoopState, LoopStatus, LoopThread};
use crate::playback::asio_state::DeviceEvents;
use crate::playback::audio_out_block::ProgramBlock;
use sp_core::audio_outputs::{AsioDest, OutputEntry};

const T0: i64 = 17_900_000_000_000_000;
const SLOT: i64 = 333_333;
const S: i64 = 10_000_000;

type Started = Arc<Mutex<Vec<LoopThread>>>;

fn out(loop_check: bool) -> AsioOut {
    let entry = OutputEntry::asio(
        "out-2",
        "SG",
        AsioDest {
            driver: "Waves SoundGrid".into(),
            channels: [0, 1],
            loop_check,
        },
    );
    AsioOut::for_entry(&entry).unwrap()
}

fn block(k: i64) -> ProgramBlock {
    ProgramBlock {
        due_100ns: T0 + k * SLOT,
        samples: Some(vec![0.25; 3200].into()),
        substituted: false,
    }
}

/// A worker whose analyzer threads are recorded, never started.
fn worker(started: &Started) -> AsioWorker {
    let sink = started.clone();
    AsioWorker::new(T0).with_loop_start(Box::new(move |t: LoopThread| sink.lock().unwrap().push(t)))
}

/// The analyzer's status as its thread would publish it.
fn publish(started: &Started, status: &LoopStatus) {
    *started.lock().unwrap()[0].status.lock().unwrap() = status.clone();
}

#[test]
fn a_loop_check_entry_taps_each_start_and_feeds_its_analyzer() {
    let started: Started = Arc::default();
    let o = out(true);
    assert!(o.spec().loop_check);
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = worker(&started);
    w.step(&o, &mut d, T0, None);
    assert_eq!(o.snapshot().state, "running");
    assert!(d.taps.is_some(), "the start took the run's taps");
    let mut feed = {
        let threads = started.lock().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "out-2");
        threads[0].feeds.try_recv().expect("the run's rings")
    };
    assert_eq!((feed.rate, feed.inputs), (96_000.0, 2));
    // The card plays: what it played and what its two inputs returned
    // (the fake's instant loop: L on both) reach the analyzer's ends.
    for k in 1..4 {
        w.step(&o, &mut d, T0 + k * SLOT + 50_000, Some(block(k)));
        d.drain(25);
    }
    let (sent, received, overflows) = feed.drained();
    assert_eq!((sent.len(), received.len(), overflows), (19_200, 19_200, 0));
    assert!(
        sent.chunks(2)
            .zip(received.chunks(2))
            .all(|(s, r)| r[0] == s[0] && r[1] == s[0])
    );
    assert!(
        sent.iter().any(|&x| x != 0.0),
        "the program reached the card"
    );
    // The analyzer's status is the output's, at the next step.
    let locked = LoopStatus {
        state: LoopState::Locked,
        delay_frames: Some(4_321),
        slips: 2,
        ..LoopStatus::default()
    };
    publish(&started, &locked);
    let t1 = T0 + 4 * SLOT;
    w.step(&o, &mut d, t1, None);
    assert_eq!(o.snapshot().status.loop_status, locked);
    // A reset closes the run: its taps go with it (the analyzer's run
    // ends once drained).
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    w.step(&o, &mut d, t1 + SLOT, None);
    assert_eq!(o.snapshot().state, "waiting");
    assert!(d.taps.is_none());
    assert_eq!(
        LoopCore::new("out-2").tick(Some(feed), T0).state,
        LoopState::Off
    );
    // While it waits, the analyzer's status still reaches the output.
    let off = LoopStatus {
        overflows: 5,
        ..LoopStatus::default()
    };
    publish(&started, &off);
    w.step(&o, &mut d, t1 + SLOT + S, None);
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.status.loop_status), ("waiting", off));
    // The reopen 2 s after the close: the second run's rings go to the
    // same thread.
    w.step(&o, &mut d, t1 + SLOT + 2 * S, None);
    assert_eq!((d.starts, o.snapshot().state), (2, "running"));
    let threads = started.lock().unwrap();
    assert_eq!(threads.len(), 1, "one analyzer thread per output");
    assert_eq!(threads[0].feeds.try_recv().map(|f| f.inputs), Ok(2));
}

#[test]
fn an_entry_without_the_loop_check_taps_nothing_and_reads_off() {
    let started: Started = Arc::default();
    let o = out(false);
    assert!(!o.spec().loop_check);
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = worker(&started);
    w.step(&o, &mut d, T0, None);
    w.step(&o, &mut d, T0 + SLOT + 50_000, Some(block(1)));
    d.drain(25);
    assert_eq!(o.snapshot().state, "running");
    assert!(d.taps.is_none() && d.pending_taps.is_none());
    assert!(started.lock().unwrap().is_empty(), "no analyzer thread");
    assert_eq!(o.snapshot().status.loop_status, LoopStatus::default());
    assert_eq!(o.snapshot().status.loop_status.state, LoopState::Off);
}
