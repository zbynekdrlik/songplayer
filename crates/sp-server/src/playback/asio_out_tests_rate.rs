//! #229, the owner's ruling (9.11.2026): PP's Waves SoundGrid with its
//! network off is off on purpose outside services, and SongPlayer must wait
//! calmly on it — no hard re-centres, no ledger losses. Measured
//! (9.11.2026, 19:23Z): it opens at 44 100 Hz / 1024 frames (96 000 Hz with
//! its network on), calls back regularly every 23.2 ms and takes ~2.9 %
//! (~29 000 ppm) more audio than its rate. The main session's option H: an
//! open at a rate other than the network's proves the card's rate silently
//! first; an open at the network's rate plays at once and is caught while
//! playing; a reopen at the rate the card was found off at proves silently.

use super::fake::FakeDevice;
use super::*;
use crate::playback::audio_out_block::ProgramBlock;
use crate::test_log::{Captured, capturing};
use sp_core::audio_outputs::{AsioDest, DanteDest, OutputEntry};

const T0: i64 = 17_900_000_000_000_000;
const SLOT: i64 = 333_333;
/// The network's rate at PP and SNV.
const NETWORK: u32 = 96_000;

fn entry() -> OutputEntry {
    OutputEntry::asio(
        "out-2",
        "SoundGrid",
        AsioDest {
            driver: "Waves SoundGrid ASIO".into(),
            channels: [0, 1],
            loop_check: false,
        },
    )
}

/// The output as the outputs task builds it: held against the network.
fn out() -> AsioOut {
    AsioOut::for_entry(&entry())
        .unwrap()
        .with_network_rate(NETWORK)
}

/// The SoundGrid driver as PP opens it (network off: 44.1 kHz; on: 96 kHz).
fn soundgrid(rate: f64) -> Opened {
    Opened {
        rate,
        buffer_frames: 1024,
        out_channels: 64,
        sample: AsioSample::Int32,
    }
}

/// The program block due at `due_100ns`.
fn block_due(due_100ns: i64) -> ProgramBlock {
    ProgramBlock {
        due_100ns,
        samples: Some(vec![0.25; 3200].into()),
        substituted: false,
    }
}

/// A card that takes `rate · (1 + ppm)` frames a second in whole buffers,
/// its debt carried across the slots and started afresh at every start.
struct Card {
    owed: f64,
    starts: u32,
}

impl Card {
    fn new() -> Self {
        Self {
            owed: 0.0,
            starts: 0,
        }
    }

    /// One slot of the card at `ppm` off its driver's rate.
    fn play(&mut self, d: &mut FakeDevice, rate: f64, ppm: f64) {
        if d.starts != self.starts {
            (self.owed, self.starts) = (0.0, d.starts);
        }
        if d.ring.is_none() {
            return;
        }
        self.owed += rate * (1.0 + ppm * 1e-6) * SLOT as f64 / 1e7;
        let n = (self.owed / f64::from(d.buffer)).floor();
        self.owed -= n * f64::from(d.buffer);
        d.drain(n as u32);
    }
}

/// What the output shows: its state, its reason code, its clock waits.
type Shown = (&'static str, Option<&'static str>, u64);

/// Run blocks `from..=to` on a card at `rate_ppm(open number)`; each change
/// of what the output shows, with its block.
fn run(
    (w, o, d, card): (&mut AsioWorker, &AsioOut, &mut FakeDevice, &mut Card),
    blocks: std::ops::RangeInclusive<i64>,
    rate_ppm: impl Fn(u32) -> (f64, f64),
) -> Vec<(i64, Shown)> {
    let mut seen: Vec<(i64, Shown)> = Vec::new();
    for k in blocks {
        let due = T0 + k * SLOT;
        w.step(o, d, due + 50_000, Some(block_due(due)));
        let s = o.snapshot();
        let now = (s.state, s.status.reason_code, s.status.clock_waits);
        if seen.last().is_none_or(|l| l.1 != now) {
            seen.push((k, now));
        }
        let (rate, ppm) = rate_ppm(d.starts);
        card.play(d, rate, ppm);
    }
    seen
}

/// The counters a fault would move, and the blocks sent.
fn faults(o: &AsioOut) -> (u64, u64, u64, u64, u64) {
    let s = o.snapshot();
    (
        s.status.hard_recentres,
        s.status.underruns,
        s.status.overflows,
        s.status.resets,
        s.blocks_sent,
    )
}

const OFF: Option<&str> = Some("clock_off_rate");

/// PP's case: SoundGrid with its network off opens at 44.1 kHz against the
/// 96 kHz network and takes 2.9 % more. From the very first open nothing
/// is fed: the output reads opening while the card's rate is measured (from
/// block 9, a quarter second of ticks after the open, a window of 500
/// buffers: 11.6 s), then waiting `clock_off_rate`, the driver kept open
/// and opened again 60 s after each open (blocks 1800 / 3602 / 5404 / 7206)
/// — never running, no hard re-centre, no underrun, no overflow, no reset,
/// no block sent, so the fault ledger (gated on running) records nothing.
/// One WARN (the wait starts), one INFO (the first open), the reopens at
/// DEBUG.
#[test]
fn soundgrid_off_its_network_waits_calmly_from_the_first_open() {
    let cap = Captured::default();
    let subscriber = capturing(&cap);
    let o = out();
    let mut d = FakeDevice::answering((0..10).map(|_| Ok(soundgrid(44_100.0))).collect());
    let mut w = AsioWorker::new(T0);
    let mut card = Card::new();
    let seen = tracing::subscriber::with_default(subscriber, || {
        w.step(&o, &mut d, T0, None);
        run((&mut w, &o, &mut d, &mut card), 1..=9000, |_| {
            (44_100.0, 29_000.0)
        })
    });
    assert_eq!(
        seen,
        [
            (1, ("opening", None, 0)),
            (358, ("waiting", OFF, 0)),
            (1800, ("waiting", OFF, 1)),
            (3602, ("waiting", OFF, 2)),
            (5404, ("waiting", OFF, 3)),
            (7206, ("waiting", OFF, 4)),
        ]
    );
    assert_eq!(faults(&o), (0, 0, 0, 0, 0));
    let s = o.snapshot();
    assert_eq!(s.status.retry_in_s, None, "the driver is open, waiting");
    assert_eq!((d.starts, d.closes), (5, 4));
    // The buffer's note at the first open, then the wait's one WARN.
    let warns = cap.lines_with(" WARN ");
    assert_eq!(warns.len(), 2, "{warns:#?}");
    assert!(
        warns[0].contains("over a third of a grid slot"),
        "{warns:#?}"
    );
    assert!(warns[1].contains("more than 1 % off"), "{warns:#?}");
    let infos = cap.lines_with(" INFO ");
    assert_eq!(infos.len(), 1, "{infos:#?}");
    assert!(infos[0].contains("opened the driver"), "{infos:#?}");
    let reopens = cap
        .lines_with("DEBUG")
        .iter()
        .filter(|l| l.contains("an off-rate card"))
        .count();
    assert_eq!(reopens, 4);
}

/// The same 2.9 % taken LESS (−29 000 ppm): the same calm wait.
#[test]
fn a_card_that_takes_too_little_off_the_network_rate_waits_calmly_too() {
    let o = out();
    let mut d = FakeDevice::answering((0..10).map(|_| Ok(soundgrid(44_100.0))).collect());
    let mut w = AsioWorker::new(T0);
    let mut card = Card::new();
    w.step(&o, &mut d, T0, None);
    let seen = run((&mut w, &o, &mut d, &mut card), 1..=3700, |_| {
        (44_100.0, -29_000.0)
    });
    assert_eq!(
        seen,
        [
            (1, ("opening", None, 0)),
            (358, ("waiting", OFF, 0)),
            (1800, ("waiting", OFF, 1)),
            (3602, ("waiting", OFF, 2)),
        ]
    );
    assert_eq!(faults(&o), (0, 0, 0, 0, 0));
}

/// SoundGrid's network comes back: the next reopen reads 96 kHz, another
/// rate than the one the card was found off at, so it is a fresh open that
/// runs at once (the network's rate: no silent proof) and plays every
/// block from there, with no fault.
#[test]
fn a_reopen_at_a_new_rate_runs_at_once() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(soundgrid(44_100.0)), Ok(soundgrid(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    let mut card = Card::new();
    w.step(&o, &mut d, T0, None);
    let seen = run((&mut w, &o, &mut d, &mut card), 1..=5400, |starts| {
        if starts < 2 {
            (44_100.0, 29_000.0)
        } else {
            (96_000.0, 0.0)
        }
    });
    assert_eq!(
        seen,
        [
            (1, ("opening", None, 0)),
            (358, ("waiting", OFF, 0)),
            (1800, ("waiting", OFF, 1)),
            (1801, ("running", None, 1)),
        ]
    );
    let s = o.snapshot();
    assert_eq!(s.status.driver_rate, 96_000);
    assert_eq!(faults(&o), (0, 0, 0, 0, 3599), "every block from 1802");
}

/// A card at another rate than the network's that takes audio AT its rate
/// (a 48 kHz card on a 96 kHz network): silent while one window proves it
/// (blocks 9 to 310: 10 s, as 500 buffers of 128 are 1.3 s), then it runs, its
/// next block priming the ring; no fault.
#[test]
fn a_card_at_its_own_rate_off_the_network_rate_proves_it_then_runs() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(fake::dvs(48_000.0))]);
    let mut w = AsioWorker::new(T0);
    let mut card = Card::new();
    w.step(&o, &mut d, T0, None);
    let seen = run((&mut w, &o, &mut d, &mut card), 1..=1800, |_| {
        (48_000.0, 120.0)
    });
    assert_eq!(
        seen,
        [(1, ("opening", None, 0)), (310, ("running", None, 0))]
    );
    assert_eq!(faults(&o), (0, 0, 0, 0, 1491));
}

/// An open at the network's rate plays at once, as before #229. A card
/// that takes 2.9 % more (or less) there is caught while playing, after two
/// off windows in a row (a stall at a window's edge is one): waiting
/// `clock_off_rate`, one WARN, and the driver opened again at once to wait
/// silently. From the verdict on nothing moves: no hard re-centre, no
/// underrun, no overflow, no block sent, a reopen every 60 s.
#[test]
fn an_off_rate_card_at_the_network_rate_is_caught_while_playing_then_waits() {
    for (ppm, caught) in [(29_000.0, 611), (-29_000.0, 611)] {
        let cap = Captured::default();
        let subscriber = capturing(&cap);
        let o = out();
        let mut d = FakeDevice::answering(vec![]);
        let mut w = AsioWorker::new(T0);
        let mut card = Card::new();
        let (seen, at_verdict) = tracing::subscriber::with_default(subscriber, || {
            w.step(&o, &mut d, T0, None);
            let mut seen = run((&mut w, &o, &mut d, &mut card), 1..=caught, |_| {
                (96_000.0, ppm)
            });
            let at_verdict = faults(&o);
            let rest = run(
                (&mut w, &o, &mut d, &mut card),
                caught + 1..=caught + 5400,
                |_| (96_000.0, ppm),
            );
            // The second run's first entry repeats what the verdict showed.
            seen.extend(rest.into_iter().filter(|(k, _)| *k != caught + 1));
            (seen, at_verdict)
        });
        // Opened again on the next block's step, then closed 60 s after each
        // open (1801 slots) and opened on the step after: every 1802 blocks.
        let reopen = caught + 1802;
        assert_eq!(
            seen,
            [
                (1, ("running", None, 0)),
                (caught, ("waiting", OFF, 1)),
                (reopen, ("waiting", OFF, 2)),
                (reopen + 1802, ("waiting", OFF, 3)),
            ],
            "{ppm} ppm"
        );
        assert_eq!(faults(&o), at_verdict, "{ppm} ppm: nothing after it");
        assert!(at_verdict.4 > 0, "{ppm} ppm: it played until the verdict");
        let off = cap.lines_with(" WARN ");
        let off = off.iter().filter(|l| l.contains("1 % off")).count();
        assert_eq!(off, 1, "{ppm} ppm");
    }
}

/// A healthy card (±300 ppm, the servo's whole range) and one just inside
/// the bound (+9 000 ppm) at the network's rate play on as before #229:
/// running from the first block for 80 s (8 windows), never a wait or a
/// reopen.
#[test]
fn a_card_within_1_percent_plays_on_as_before() {
    for ppm in [300.0, -300.0, 9_000.0, -9_000.0] {
        let o = out();
        let mut d = FakeDevice::answering(vec![]);
        let mut w = AsioWorker::new(T0);
        let mut card = Card::new();
        w.step(&o, &mut d, T0, None);
        let seen = run((&mut w, &o, &mut d, &mut card), 1..=2400, |_| {
            (96_000.0, ppm)
        });
        assert_eq!(seen, [(1, ("running", None, 0))], "{ppm} ppm");
        assert_eq!(d.starts, 1, "{ppm} ppm");
    }
}

/// A healthy card leaves the run as it was: the +300 ppm card's 80 s
/// have no hard re-centre, no underrun, every block sent.
#[test]
fn a_healthy_card_has_no_fault() {
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    let mut card = Card::new();
    w.step(&o, &mut d, T0, None);
    run((&mut w, &o, &mut d, &mut card), 1..=2400, |_| {
        (96_000.0, 300.0)
    });
    assert_eq!(faults(&o), (0, 0, 0, 0, 2400));
}

/// A Dante output is never held against the network's rate: its device
/// takes the program's 48 kHz by design (dlink converts). An ASIO entry
/// built by the outputs task carries the rate across the host's pipe.
#[test]
fn only_an_asio_output_is_held_against_the_network_rate() {
    let dante = OutputEntry::dante(
        "out-4",
        "Dante",
        DanteDest {
            device: "songplayer".into(),
            ip: "10.0.0.5".into(),
        },
    );
    assert_eq!(
        AsioOut::for_dante(&dante).unwrap().spec().network_rate,
        None
    );
    assert_eq!(
        AsioOut::for_entry(&entry()).unwrap().spec().network_rate,
        None
    );
    assert_eq!(out().spec().network_rate, Some(NETWORK));
    assert_eq!(AsioOut::from_spec(&out().spec()).spec(), out().spec());
}

/// Only a SILENT run takes its tick base at its first step: a run that
/// plays takes it at its priming, as before #229. A driver that calls back
/// a few times after the open's first step but before the first block, and
/// then never again, gives no clock: it waits (block 60, 2 s after the
/// open), never a stall and a reset.
#[test]
fn a_playing_run_takes_its_tick_base_at_its_priming() {
    let o = out();
    let mut d = FakeDevice::answering(vec![]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    w.step(&o, &mut d, T0 + 10_000, None);
    d.drain(3);
    let mut seen: Vec<(i64, Shown)> = Vec::new();
    for k in 1..=200 {
        let due = T0 + k * SLOT;
        w.step(&o, &mut d, due + 50_000, Some(block_due(due)));
        let s = o.snapshot();
        let now = (s.state, s.status.reason_code, s.status.clock_waits);
        if seen.last().is_none_or(|l| l.1 != now) {
            seen.push((k, now));
        }
    }
    let no_clock = Some("no_clock");
    assert_eq!(
        seen,
        [(1, ("running", None, 0)), (60, ("waiting", no_clock, 0))]
    );
    assert_eq!(o.snapshot().status.resets, 0);
}
