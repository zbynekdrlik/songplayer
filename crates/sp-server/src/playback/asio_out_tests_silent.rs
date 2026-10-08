//! #233, found live at PP (8.10.2026 13:10Z, 0.74.0): Dante Virtual
//! Soundcard there opens and never calls back (no Dante PTP clock on PP's
//! network). The worker went "no callback from the driver for 2 s" → reopen
//! with the 2 / 10 / 30 / 60 s backoff, and every run counted hard
//! re-centres. The owner's ruling (#233, 8.10.2026): a driver that opens and
//! does not tick is a calm, visible WAIT, never a fault loop, and the output
//! starts by itself once it ticks (`asio_out_tests_clock.rs`). A driver
//! that ticked and then stops is a stall, as before.

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

/// A change of what the output shows: the block, its state, its reason
/// code and its clock waits.
type Seen = (i64, &'static str, Option<&'static str>, u64);

/// A driver that opens and never calls back (the owner's ruling, #233,
/// 8.10.2026):
/// - its first 2 s read running (a driver normally ticks within ms), then
///   waiting, reason `no_clock`, the driver kept open: block 60 (60 slots +
///   5 ms after the open at T0 is 2.005 s; block 59 is 1.972 s);
/// - with still no callback 60 s after an open, the driver is closed and
///   opened again at once: block 1800 (60.005 s), the reopen on block
///   1801's step (that block dropped), then every 1801 blocks (blocks 3602,
///   5404, 7206), no backoff;
/// - it reads waiting through every reopen, and `clock_waits` counts them;
/// - every run primes the ring with its first block (no re-centre) and the
///   rest wait for a callback: no reset, no hard re-centre, no overflow,
///   one block sent per run (5 runs in 5 min).
#[test]
fn a_driver_that_never_calls_back_waits_calmly_for_its_clock() {
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    let mut seen: Vec<Seen> = Vec::new();
    for k in 1..=9000 {
        let due = T0 + k * SLOT;
        w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
        let s = o.snapshot();
        let now: Seen = (k, s.state, s.status.reason_code, s.status.clock_waits);
        if seen
            .last()
            .is_none_or(|l| (l.1, l.2, l.3) != (now.1, now.2, now.3))
        {
            seen.push(now);
        }
    }
    let no_clock = Some("no_clock");
    assert_eq!(
        seen,
        [
            (1, "running", None, 0),
            (60, "waiting", no_clock, 0),
            (1800, "waiting", no_clock, 1),
            (3602, "waiting", no_clock, 2),
            (5404, "waiting", no_clock, 3),
            (7206, "waiting", no_clock, 4),
        ]
    );
    let s = o.snapshot();
    assert_eq!(
        (
            s.status.resets,
            s.status.hard_recentres,
            s.status.overflows,
            s.blocks_sent
        ),
        (0, 0, 0, 5),
        "{s:?}"
    );
    assert_eq!(s.status.retry_in_s, None, "the driver is open, waiting");
    assert_eq!((d.starts, d.closes, d.callbacks), (5, 4, 0));
}

/// A driver that ticked and then stops is a stall, as before the ruling:
/// four such runs in a row (each primes, the card plays one slot, then
/// nothing), each closed 2 s after its last callback with the reason
/// `stalled`, `resets` counting them, and the next try 2, 10, 30, then 60 s
/// later (no run lasts the 60 s that resets the backoff).
#[test]
fn a_driver_that_ticked_and_stops_stalls_with_the_escalating_backoff() {
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    let mut t = T0;
    let mut retries = Vec::new();
    for n in 1..=4u64 {
        w.step(&o, &mut d, t, None);
        assert_eq!(o.snapshot().state, "running", "run {n} opened");
        let mut blocks = 0;
        while o.snapshot().state == "running" {
            blocks += 1;
            assert!(blocks <= 100, "run {n}: the stall never closed it");
            let due = t + blocks * SLOT;
            w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
            if blocks == 1 {
                d.drain(25);
            }
        }
        let s = o.snapshot();
        assert_eq!(s.status.resets, n, "run {n}: {s:?}");
        assert_eq!(s.reason.as_ref().map(Reason::code), Some("stalled"));
        let retry = s.status.retry_in_s.expect("tried again");
        retries.push(retry);
        t += blocks * SLOT + 50_000 + (retry * 1e7) as i64;
    }
    assert_eq!(retries, [2.0, 10.0, 30.0, 60.0]);
    assert_eq!(o.snapshot().status.clock_waits, 0, "no clock wait");
}

/// A card that starts late: block 2 comes before its first callback and
/// waits (the ring keeps its priming); the card then plays one slot, and
/// from block 3 on the blocks go through as usual: every block but block 2
/// sent, no hard re-centre, no underrun.
#[test]
fn a_card_that_calls_back_late_goes_on_from_its_priming() {
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    for k in 1..=2 {
        let due = T0 + k * SLOT;
        w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
    }
    assert_eq!(o.snapshot().blocks_sent, 1, "block 2 waited");
    d.drain(25);
    for k in 3..=200 {
        let due = T0 + k * SLOT;
        w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
        d.drain(25);
    }
    let s = o.snapshot();
    assert_eq!(
        (
            s.state,
            s.blocks_sent,
            s.status.hard_recentres,
            s.status.underruns
        ),
        ("running", 199, 0, 0),
        "{s:?}"
    );
}
