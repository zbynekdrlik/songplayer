//! #233 (PP's Waves SoundGrid output pops), measurement first: a driver's
//! resync request (`kAsioResyncRequest`, answered "supported") is counted
//! since the output was built and WARNed, and changes nothing else: the
//! drift servo, the ring and the driver run on (no rebase, no priming,
//! never a close or a reopen). What to do about one is decided on PP's
//! measured evidence.

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
/// (the worker tests' pins). A resync is counted, and the output runs on
/// as if it had not come: the same open, the same servo figures at once,
/// and — against a twin worker fed the same blocks with no resync — the
/// same status (but the count) and the same samples played 60 blocks on.
#[test]
fn a_resync_request_is_counted_and_leaves_the_running_output_untouched() {
    let o = out();
    let twin_out = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut twin = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    let mut tw = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    tw.step(&twin_out, &mut twin, T0, None);
    run(&mut w, &o, &mut d, 1, 150);
    run(&mut tw, &twin_out, &mut twin, 1, 150);
    let s = o.snapshot().status;
    assert!((s.latency_ms - 70.625).abs() < 1e-9, "{s:?}");
    assert_eq!((s.offset_ms, s.resyncs), (-0.0416, 0), "{s:?}");
    d.events.push_back(resync());
    w.step(&o, &mut d, T0 + 151 * SLOT, None);
    tw.step(&twin_out, &mut twin, T0 + 151 * SLOT, None);
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.reason.clone()), ("running", None));
    assert_eq!(snap.status.resyncs, 1, "{snap:?}");
    assert_eq!(
        (d.closes, d.starts, d.opened.len()),
        (0, 1, 1),
        "a resync never closes or reopens the driver"
    );
    assert!(
        (snap.status.latency_ms - 70.625).abs() < 1e-9,
        "the servo is not reset: {snap:?}"
    );
    assert_eq!(
        (snap.status.offset_ms, snap.status.slew_eta_s),
        (-0.0416, None),
        "{snap:?}"
    );
    run(&mut w, &o, &mut d, 151, 60);
    run(&mut tw, &twin_out, &mut twin, 151, 60);
    let snap = o.snapshot();
    assert_eq!(snap.state, "running");
    assert_eq!(
        AsioStatus {
            resyncs: 0,
            ..snap.status.clone()
        },
        twin_out.snapshot().status,
        "nothing but the count differs from the twin"
    );
    assert_eq!(snap.status.resyncs, 1);
    assert!(d.played == twin.played, "the card played the same samples");
    assert_eq!(
        (snap.status.hard_recentres, snap.status.underruns, d.closes),
        (0, 0, 0)
    );
}

/// The count adds up across runs, with a resync in each (a reset between
/// them), and moves no other counter: each run makes one hard re-centre
/// before its resync (150 blocks, then a block 65 ms "early", a deficit,
/// `asio_out_tests.rs::a_hard_re_centre_is_counted_and_shown_across_runs`)
/// and none after it.
#[test]
fn resyncs_add_up_across_runs_and_move_no_other_counter() {
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
        // The next block, 65 ms over the target: left to the ratio.
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
