//! #233 (the main session's ruling, comment 6056680979, Q1): the worker
//! hands the servo the card's underrun frames — its underrun callbacks ×
//! the driver's buffer, a short callback counted whole — so the excess an
//! underrun leaves is kept as cushion, and the status shows it.

use super::fake::{FakeDevice, dvs};
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

fn block(k: i64) -> ProgramBlock {
    ProgramBlock {
        due_100ns: T0 + k * SLOT,
        samples: Some(vec![0.25; 3200].into()),
        substituted: false,
    }
}

/// After 150 blocks handled 5 ms late (the ring at 5 436 frames, the worker
/// tests' pin), block 151 comes 70 ms later: the card takes 52 more
/// callbacks first, the 43rd short and 9 dry — 10 underruns, 1 280 frames as
/// the worker counts them (the true silence is 1 220). The block reads 80 ms
/// (the ring empty + the hold + 75 ms late): 13.3 ms over the target, under
/// the last resort's 4 slots, and kept as cushion — 13.3333 ms, 10 × 128
/// frames.
#[test]
fn an_underruns_excess_shows_as_the_outputs_cushion() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    for k in 1..151 {
        w.step(&o, &mut d, T0 + k * SLOT + 50_000, Some(block(k)));
        d.drain(25);
    }
    assert_eq!(d.ring_frames(), 5_436);
    assert_eq!(o.snapshot().status.cushion_ms, 0.0);
    d.drain(52);
    assert_eq!(d.underruns, 10);
    w.step(&o, &mut d, T0 + 151 * SLOT + 750_000, Some(block(151)));
    let s = o.snapshot().status;
    assert_eq!((s.cushion_ms, s.hard_recentres), (13.3333, 0), "{s:?}");
}
