//! #233 (PP's Waves SoundGrid output pops): the driver's callback timing on
//! `outputs[i].asio` — the longest gap between two buffer switches since the
//! output was built, the late and fast ones counted across polls and runs
//! (a run's device counts from 0 again), and the last late one with its
//! age, ageing while the output waits too.

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
            loop_check: false,
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

/// A poll's callback timing: its longest gap, and the NEW late and fast
/// buffer switches (the fake counts them per run, like a slot).
fn timing(gap_100ns: i64, late: u64, fast: u64) -> DeviceEvents {
    DeviceEvents {
        callback_gap_max_100ns: gap_100ns,
        late_callbacks: late,
        fast_callbacks: fast,
        ..Default::default()
    }
}

/// The status's timing figures: (longest gap ms, late, fast, last late).
type Shown = (f64, u64, u64, Option<LateCallback>);

fn shown(o: &AsioOut) -> Shown {
    let s = o.snapshot().status;
    (
        s.callback_gap_max_ms,
        s.late_callbacks,
        s.fast_callbacks,
        s.last_late_callback,
    )
}

fn late(gap_ms: f64, ago_s: f64) -> Option<LateCallback> {
    Some(LateCallback { gap_ms, ago_s })
}

#[test]
fn the_callback_timing_adds_up_across_polls_and_runs() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    for k in 1..4 {
        w.step(&o, &mut d, T0 + k * SLOT + 50_000, Some(block(k)));
        d.drain(25);
    }
    assert_eq!(shown(&o), (0.0, 0, 0, None));
    // One late and two fast buffer switches, the longest gap 3.1 ms.
    let t1 = T0 + 4 * SLOT;
    d.events.push_back(timing(31_000, 1, 2));
    w.step(&o, &mut d, t1, None);
    d.drain(25);
    assert_eq!(shown(&o), (3.1, 1, 2, late(3.1, 0.0)));
    // A second on: a shorter gap and one more fast one, no late one — the
    // longest gap and the last late one stay, the late one a second older.
    d.events.push_back(timing(20_000, 0, 1));
    w.step(&o, &mut d, t1 + S, None);
    d.drain(25);
    assert_eq!(shown(&o), (3.1, 1, 3, late(3.1, 1.0)));
    // Another late one, 12.3456 ms.
    d.events.push_back(timing(123_456, 1, 0));
    w.step(&o, &mut d, t1 + 2 * S, None);
    d.drain(25);
    assert_eq!(shown(&o), (12.3456, 2, 3, late(12.3456, 0.0)));
    // A reset closes the run with its counts; the last late one ages while
    // the output waits for its reopen 2 s later.
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    w.step(&o, &mut d, t1 + 3 * S, None);
    assert_eq!(o.snapshot().state, "waiting");
    assert_eq!(shown(&o), (12.3456, 2, 3, late(12.3456, 1.0)));
    w.step(&o, &mut d, t1 + 4 * S, None);
    assert_eq!(o.snapshot().state, "waiting");
    assert_eq!(shown(&o), (12.3456, 2, 3, late(12.3456, 2.0)));
    // The next run's driver counts from 0 again: its counts add to the
    // closed run's; its late one is the last one now.
    w.step(&o, &mut d, t1 + 5 * S, None);
    assert_eq!((d.starts, o.snapshot().state), (2, "running"));
    d.events.push_back(timing(50_000, 1, 1));
    w.step(&o, &mut d, t1 + 5 * S + SLOT, None);
    assert_eq!(shown(&o), (12.3456, 3, 4, late(5.0, 0.0)));
}

/// #289 step 0: the device's own output latency (its driver's frames at its
/// rate): none before the first open, read at the open, re-read on
/// `kAsioLatenciesChanged`, kept while the output waits to reopen.
#[test]
fn the_device_latency_is_the_drivers_own_and_follows_its_change() {
    let o = out();
    assert_eq!(o.snapshot().status.device_latency_ms, None);
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    // The fake driver reports 128 frames at 96 kHz.
    assert_eq!(o.snapshot().status.device_latency_ms, Some(128.0 / 96.0));
    for k in 1..4 {
        w.step(&o, &mut d, T0 + k * SLOT + 50_000, Some(block(k)));
        d.drain(25);
    }
    assert_eq!(o.snapshot().status.device_latency_ms, Some(128.0 / 96.0));
    d.latency = 480;
    d.events.push_back(DeviceEvents {
        latencies_changed: true,
        ..Default::default()
    });
    w.step(&o, &mut d, T0 + 4 * SLOT, None);
    assert_eq!(o.snapshot().status.device_latency_ms, Some(5.0));
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    w.step(&o, &mut d, T0 + 5 * SLOT, None);
    let s = o.snapshot();
    assert_eq!(
        (s.state, s.status.device_latency_ms),
        ("waiting", Some(5.0))
    );
}

/// Block `k` handed a slot ahead of its stamp (#289 step 2: the pacer hands
/// each pair out at its commit): when it is handled, and the block.
fn ahead(k: i64) -> (i64, ProgramBlock) {
    (T0 + (k - 1) * SLOT + 5_000, block(k))
}

/// #289 step 2 regression (resolume-snv, 26.11.2026 05:52:38 Bratislava:
/// the first start on step 2 logged `cause="deficit" ms=30.6333
/// lateness_ms=-32.8209` on out-4). At a start the program commits the
/// fill of S − 1 and the first playlist boundary S at the same instant (S
/// is handed a slot ahead), so the output gets both back to back, before
/// the driver's first callback. The second block belongs to the same
/// hand-off: it is played, never dropped, and the steady blocks a slot
/// ahead after it hold the target with no hard re-centre.
#[test]
fn a_start_pair_handed_back_to_back_primes_with_no_hard_recentre() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    // The fill of b(1), handled at its boundary, primes the ring; b(2), a
    // slot ahead, comes 0.1 ms later with no callback in between.
    w.step(&o, &mut d, T0 + SLOT + 4_000, Some(block(1)));
    w.step(&o, &mut d, T0 + SLOT + 5_000, Some(block(2)));
    d.drain(25);
    for k in 3..3 + 30 * 5 {
        let (at, b) = ahead(k);
        w.step(&o, &mut d, at, Some(b));
        d.drain(25);
    }
    let snap = o.snapshot();
    let s = &snap.status;
    assert_eq!(
        (s.hard_recentres, s.last_hard_recentre.is_some()),
        (0, false),
        "{s:?}"
    );
    assert_eq!((snap.blocks_sent, snap.blocks_dropped), (152, 0));
    assert_eq!((d.underruns, s.overflows), (0, 0));
}

/// The PP rule stays (#233): a driver that never calls back after the
/// priming takes the start's hand-off, never a pile-up: a block handled a
/// slot or more after the priming waits for the driver's first tick.
#[test]
fn a_driver_with_no_callback_after_the_priming_takes_no_later_block() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    // The pair at one instant, so b(3) comes exactly one slot after the
    // priming: the first block that waits.
    w.step(&o, &mut d, T0 + SLOT + 5_000, Some(block(1)));
    w.step(&o, &mut d, T0 + SLOT + 5_000, Some(block(2)));
    let handed = d.ring_frames();
    for k in 3..10 {
        let (at, b) = ahead(k);
        w.step(&o, &mut d, at, Some(b));
    }
    assert_eq!(d.ring_frames(), handed, "no block after the hand-off");
    assert_eq!(o.snapshot().blocks_sent, 2);
}
