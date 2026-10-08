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
