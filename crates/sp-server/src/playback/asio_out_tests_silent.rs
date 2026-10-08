//! #233, found live at PP (8.10.2026 13:10Z, 0.74.0): Dante Virtual
//! Soundcard there (unlicensed) opens and never calls back. The worker went
//! "no callback from the driver for 2 s" → reopen, and every run counted
//! hard re-centres: the blocks after the priming piled up in a ring the card
//! never took (an excess re-centre every 4 slots). Opening, priming and a
//! card that has not called back yet are no fault: such a driver shows its
//! resets and no hard re-centre.

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

/// A driver that opens and never calls back, four runs in a row. Each run's
/// first block primes the ring (no re-centre) and the blocks after it wait
/// for the card's first callback, so nothing piles up: no hard re-centre,
/// no overflow, one block sent per run. No callback for 2 s closes the run:
/// the first poll is block 1's, so at block 62, 61 slots later (60 slots
/// are 2 µs short of 2 s). No run lasts the 60 s that resets the backoff,
/// so the next try waits 2, 10, 30, then 60 s.
#[test]
fn a_driver_that_never_calls_back_resets_with_no_hard_re_centre() {
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
        }
        assert_eq!(blocks, 62, "run {n}: 2 s with no callback");
        let s = o.snapshot();
        assert_eq!(
            (
                s.status.resets,
                s.status.hard_recentres,
                s.status.overflows,
                s.blocks_sent
            ),
            (n, 0, 0, n),
            "run {n}: {s:?}"
        );
        assert_eq!(s.reason.as_ref().map(Reason::code), Some("stalled"));
        let retry = s.status.retry_in_s.expect("tried again");
        retries.push(retry);
        t += blocks * SLOT + 50_000 + (retry * 1e7) as i64;
    }
    assert_eq!(retries, [2.0, 10.0, 30.0, 60.0]);
    assert_eq!((d.starts, d.callbacks), (4, 0));
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
