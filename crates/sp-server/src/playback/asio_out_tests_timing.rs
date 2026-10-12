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
