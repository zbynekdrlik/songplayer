//! #233 (PP's Waves SoundGrid output pops): a driver's resync request
//! (`kAsioResyncRequest`, answered "supported") is acted on — the drift
//! servo starts afresh and its next block primes the ring, as at an open —
//! and counted since the output was built; it never closes the output (a
//! SoundGrid reopen takes ~10 s). The old servo's hard re-centres stay
//! counted.

use super::fake::{FakeDevice, dvs};
use super::*;
use crate::playback::audio_out_block::ProgramBlock;
use sp_core::audio_outputs::{AsioDest, OutputEntry};

const T0: i64 = 17_900_000_000_000_000;
const SLOT: i64 = 333_333;
const S: i64 = 10_000_000;

fn out() -> AsioOut {
    let entry = OutputEntry::asio(
        "out-3",
        "DVS",
        AsioDest {
            driver: "Dante Virtual Soundcard (x64)".into(),
            channels: [0, 1],
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

/// `n` boundaries from `from`, each handled 5 ms late, the card draining 25
/// callbacks (96 kHz / 128 frames) after each (`asio_out_tests.rs::run`).
fn run(w: &mut AsioWorker, o: &AsioOut, d: &mut FakeDevice, from: i64, n: i64) {
    for k in from..from + n {
        w.step(o, d, T0 + k * SLOT + 50_000, Some(block(k)));
        d.drain(25);
    }
}

fn resync() -> DeviceEvents {
    DeviceEvents {
        resync: true,
        ..Default::default()
    }
}

/// After 150 blocks the servo reads 70.625 ms with an offset of −0.0416 ms
/// (the worker tests' pins). A resync starts it afresh: no window measured
/// yet, so the latency and the offset read 0, while the output keeps
/// running on the same open. Its next block primes the ring (no re-centre
/// counted) and the next windows measure the target again.
#[test]
fn a_resync_request_rebases_the_servo_and_never_reopens() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    run(&mut w, &o, &mut d, 1, 150);
    let s = o.snapshot().status;
    assert!((s.latency_ms - 70.625).abs() < 1e-9, "{s:?}");
    assert_eq!((s.offset_ms, s.resyncs), (-0.0416, 0), "{s:?}");
    d.events.push_back(resync());
    w.step(&o, &mut d, T0 + 151 * SLOT, None);
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.reason.clone()), ("running", None));
    assert_eq!(snap.status.resyncs, 1, "{snap:?}");
    assert_eq!(
        (d.closes, d.starts, d.opened.len()),
        (0, 1, 1),
        "a resync never closes or reopens the driver"
    );
    assert_eq!(
        (snap.status.latency_ms, snap.status.offset_ms),
        (0.0, 0.0),
        "the servo starts afresh: {snap:?}"
    );
    run(&mut w, &o, &mut d, 151, 60);
    let snap = o.snapshot();
    let s = &snap.status;
    assert_eq!(snap.state, "running");
    assert_eq!(
        (s.hard_recentres, s.underruns, s.resets, s.resyncs),
        (0, 0, 0, 1),
        "the priming is no re-centre: {s:?}"
    );
    assert!(
        (s.latency_ms - 70.625).abs() < 0.1,
        "measured at the target again: {s:?}"
    );
    assert_eq!(d.closes, 0);
}

/// The count adds up across runs, with a resync in each, and the hard
/// re-centre each run made before its resync stays counted (the rebased
/// servo counts from 0). Each run: 150 blocks, a block 65 ms "early" (a
/// deficit re-centre, `asio_out_tests.rs::a_hard_re_centre_is_counted_and_shown_across_runs`),
/// then a resync; a reset between the runs.
#[test]
fn resyncs_add_up_across_runs_and_keep_the_hard_re_centres() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    for (run_no, from) in [(1_u64, 1), (2, 215)] {
        w.step(&o, &mut d, T0 + (from - 1) * SLOT, None);
        assert_eq!(o.snapshot().state, "running");
        run(&mut w, &o, &mut d, from, 150);
        let k = from + 150;
        let early = ProgramBlock {
            due_100ns: T0 + k * SLOT + 650_000,
            ..block(k)
        };
        w.step(&o, &mut d, T0 + k * SLOT + 50_000, Some(early));
        d.drain(25);
        assert_eq!(o.snapshot().status.hard_recentres, run_no);
        d.events.push_back(resync());
        w.step(&o, &mut d, T0 + (k + 1) * SLOT, None);
        let snap = o.snapshot();
        assert_eq!(snap.state, "running");
        assert_eq!(
            (snap.status.resyncs, snap.status.hard_recentres),
            (run_no, run_no),
            "{snap:?}"
        );
        // The rebased servo's first block primes: still no new re-centre.
        run(&mut w, &o, &mut d, k + 1, 1);
        assert_eq!(o.snapshot().status.hard_recentres, run_no);
        if run_no == 1 {
            d.events.push_back(DeviceEvents {
                reset: true,
                ..Default::default()
            });
            w.step(&o, &mut d, T0 + (k + 2) * SLOT, None);
            let s = o.snapshot().status;
            assert_eq!((s.resets, s.resyncs, s.hard_recentres), (1, 1, 1));
            // The reopen is due 2 s on: before the next run's first step.
            assert_eq!(s.retry_in_s, Some(2.0));
            assert!(k + 2 + 2 * S / SLOT < 214);
        }
    }
    assert_eq!((d.starts, d.closes), (2, 1));
}
