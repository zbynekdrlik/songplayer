//! The owner's ruling (#233, 8.10.2026), verbatim in part: "… no to by
//! nemalo skončiť v režime, že sa to nejak zblbne, zasekne, zacyklí alebo
//! crashne, normálne by to malo ísť a keď DVS sa rozbehne, tak by to malo
//! začať samo fungovať". At PP, DVS opens and never calls back while PP's
//! network has no Dante PTP clock: the output waits for it calmly, and runs
//! by itself once the driver ticks.

use std::sync::{Arc, Mutex};

use super::fake::FakeDevice;
use super::*;
use crate::playback::audio_out_block::ProgramBlock;
use sp_core::audio_outputs::{AsioDest, OutputEntry};

const T0: i64 = 17_900_000_000_000_000;
const SLOT: i64 = 333_333;

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

/// The program block due at `due_100ns`.
fn block_due(due_100ns: i64) -> ProgramBlock {
    ProgramBlock {
        due_100ns,
        samples: Some(vec![0.25; 3200].into()),
        substituted: false,
    }
}

/// The log lines a scoped subscriber wrote (this test's thread only).
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// DVS silent for 5 min (9 000 blocks: it waits for its clock from block
/// 60 and is opened again at blocks 1801, 3603, 5405 and 7207), then it
/// ticks from block 9001 on (the card plays one slot after each block). The
/// output starts by itself: block 9001's step has not seen a callback yet,
/// block 9002's has, so it runs from there, well within 60 s, with no
/// operator action, and holds a minute of blocks.
/// - The servo starts afresh when the clock arrives: its run so far
///   observed one block, a minute earlier.
/// - The whole time: no reset, no hard re-centre, no underrun.
/// - Every block from 9002 on is sent: 5 primes + 1 799.
/// - The log: ONE WARN (the wait starts) and two INFO (the first open; the
///   clock arriving); the four reopens and their closes only at DEBUG.
#[test]
fn a_driver_silent_for_5_min_runs_by_itself_once_it_ticks() {
    let cap = Captured::default();
    let writer = cap.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    let mut running_from = None;
    tracing::subscriber::with_default(subscriber, || {
        w.step(&o, &mut d, T0, None);
        for k in 1..=9000 {
            let due = T0 + k * SLOT;
            w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
        }
        let s = o.snapshot();
        assert_eq!(
            (s.state, s.status.reason_code, s.status.clock_waits),
            ("waiting", Some("no_clock"), 4),
            "silent: {s:?}"
        );
        for k in 9001..=10_800 {
            let due = T0 + k * SLOT;
            w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
            if running_from.is_none() && o.snapshot().state == "running" {
                running_from = Some(k);
            }
            d.drain(25);
        }
    });
    assert_eq!(running_from, Some(9002), "runs once the driver ticks");
    let s = o.snapshot();
    assert_eq!(
        (s.state, s.reason.as_ref().map(Reason::code)),
        ("running", None),
        "{s:?}"
    );
    assert_eq!(
        (
            s.status.resets,
            s.status.hard_recentres,
            s.status.underruns,
            s.status.clock_waits,
            s.blocks_sent
        ),
        (0, 0, 0, 4, 1_804),
        "{s:?}"
    );
    let text = String::from_utf8(cap.0.lock().unwrap().clone()).unwrap();
    let at = |level: &str| -> Vec<String> {
        text.lines()
            .filter(|l| l.contains(level))
            .map(str::to_string)
            .collect()
    };
    let warns = at(" WARN ");
    assert_eq!(warns.len(), 1, "{warns:#?}");
    assert!(warns[0].contains("waits for its clock"), "{warns:#?}");
    let infos = at(" INFO ");
    assert_eq!(infos.len(), 2, "{infos:#?}");
    assert!(infos[0].contains("opened the driver"), "{infos:#?}");
    assert!(infos[1].contains("gives a clock now"), "{infos:#?}");
    let debugs = at("DEBUG");
    let reopens = debugs
        .iter()
        .filter(|l| l.contains("still no clock 60 s after the open"))
        .count();
    let opened_again = debugs
        .iter()
        .filter(|l| l.contains("opened the driver again"))
        .count();
    assert_eq!((reopens, opened_again), (4, 4), "{debugs:#?}");
}

/// A subscriber writing every event at DEBUG and above into `cap`.
fn capturing(cap: &Captured) -> impl tracing::Subscriber + Send + Sync + 'static {
    let writer = cap.clone();
    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish()
}

/// The lines `cap` holds at `level` (" WARN ", " INFO ", "DEBUG").
fn lines_at(cap: &Captured, level: &str) -> Vec<String> {
    String::from_utf8(cap.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter(|l| l.contains(level))
        .map(str::to_string)
        .collect()
}

/// What the output shows: its state, reason code and clock waits.
type Shown = (&'static str, Option<&'static str>, u64);

/// Review round 9: a driver that calls back a few times at EVERY open
/// (before the run's first block) and then never again gives no clock
/// through its reopens too. A burst before the priming is no clock, even
/// while the output waits for one: it keeps reading waiting through every
/// reopen (blocks 1800 and 3602), with one WARN and no INFO but the first
/// open's.
#[test]
fn a_burst_at_every_reopen_is_no_clock() {
    let cap = Captured::default();
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    let mut seen: Vec<(i64, Shown)> = Vec::new();
    tracing::subscriber::with_default(capturing(&cap), || {
        w.step(&o, &mut d, T0, None);
        d.drain(3);
        for k in 1..=4000 {
            let starts = d.starts;
            let due = T0 + k * SLOT;
            w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
            if d.starts > starts {
                d.drain(3);
            }
            let s = o.snapshot();
            let now: Shown = (s.state, s.status.reason_code, s.status.clock_waits);
            if seen.last().is_none_or(|l| l.1 != now) {
                seen.push((k, now));
            }
        }
    });
    let no_clock = Some("no_clock");
    assert_eq!(
        seen,
        [
            (1, ("running", None, 0)),
            (60, ("waiting", no_clock, 0)),
            (1800, ("waiting", no_clock, 1)),
            (3602, ("waiting", no_clock, 2)),
        ]
    );
    assert_eq!(d.starts, 3);
    let warns = lines_at(&cap, " WARN ");
    assert_eq!(warns.len(), 1, "{warns:#?}");
    let infos = lines_at(&cap, " INFO ");
    assert_eq!(infos.len(), 1, "only the first open: {infos:#?}");
}

/// Review round 9: when the clock arrives the servo starts afresh, so the
/// block it observes first PRIMES it to the target. Here the card, once it
/// ticks, first takes 60 callbacks at once: the ring the first block primed
/// is left with 956 frames, a latency of 19.96 ms, under the 38.3 ms floor.
/// The run's old servo would count that as a deficit hard re-centre; the
/// fresh one only primes, and the output then runs with no underrun.
#[test]
fn a_clock_that_arrives_starts_a_fresh_servo() {
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    for k in 1..=90 {
        let due = T0 + k * SLOT;
        w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
    }
    assert_eq!(o.snapshot().status.reason_code, Some("no_clock"));
    d.drain(60);
    for k in 91..=200 {
        let due = T0 + k * SLOT;
        w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
        d.drain(25);
    }
    let s = o.snapshot();
    assert_eq!(
        (
            s.state,
            s.status.hard_recentres,
            s.status.underruns,
            s.blocks_sent
        ),
        ("running", 0, 0, 111),
        "{s:?}"
    );
}

/// Review round 9: another close during a wait for the clock (here the
/// driver asks for a reset) ends that wait: the close is a reset with its
/// backoff, and the reopen 2 s later is a fresh open — running, its INFO —
/// not a reopen of the wait (`waiting`, `no_clock`, DEBUG).
#[test]
fn a_reset_during_a_wait_for_the_clock_ends_the_wait() {
    let cap = Captured::default();
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    tracing::subscriber::with_default(capturing(&cap), || {
        w.step(&o, &mut d, T0, None);
        for k in 1..=60 {
            let due = T0 + k * SLOT;
            w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
        }
        assert_eq!(o.snapshot().status.reason_code, Some("no_clock"));
        d.events.push_back(DeviceEvents {
            reset: true,
            ..Default::default()
        });
        let at = T0 + 61 * SLOT + 50_000;
        w.step(&o, &mut d, at, None);
        let s = o.snapshot();
        assert_eq!(
            (s.state, s.status.reason_code, s.status.resets),
            ("waiting", Some("reset"), 1),
            "{s:?}"
        );
        w.step(&o, &mut d, at + 20_000_000, None);
    });
    let s = o.snapshot();
    assert_eq!(
        (s.state, s.reason.as_ref().map(Reason::code)),
        ("running", None),
        "{s:?}"
    );
    let infos = lines_at(&cap, " INFO ");
    assert_eq!(infos.len(), 2, "two opens: {infos:#?}");
    assert!(infos[1].contains("opened the driver"), "{infos:#?}");
}

/// Review round 10: in production the program's blocks reach the worker
/// through the output's queue, and an open that takes longer than a slot
/// finds some queued, which it drops as stale. While the output waits for
/// its clock a reopen is a retry: its stale count logs at DEBUG too, so a
/// minute-by-minute wait writes no INFO (two blocks queued before each of
/// the reopens at blocks 1801 and 3603).
#[test]
fn a_reopen_during_a_wait_logs_its_stale_blocks_at_debug() {
    let cap = Captured::default();
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    tracing::subscriber::with_default(capturing(&cap), || {
        w.step(&o, &mut d, T0, None);
        for k in 1..=4000 {
            let due = T0 + k * SLOT;
            if k == 1801 || k == 3603 {
                o.push(block_due(due - SLOT));
                o.push(block_due(due));
            }
            w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
        }
    });
    assert_eq!(d.starts, 3);
    assert_eq!(o.queued(), 0, "each reopen dropped them");
    let infos = lines_at(&cap, " INFO ");
    assert_eq!(infos.len(), 1, "only the first open: {infos:#?}");
    let stale = lines_at(&cap, "DEBUG")
        .into_iter()
        .filter(|l| l.contains("dropped the blocks queued while the driver opened"))
        .count();
    assert_eq!(stale, 2);
}
