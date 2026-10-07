//! #233: the ASIO worker over a scripted driver — it opens at the driver's
//! rate and sets nothing; blocks reach the card through the servo, the
//! resampler and the ring with no underrun and the ring holds the target; a
//! reset, a stall and a rate change close it and it reopens after the
//! backoff (a 60 s run resets it); a busy driver is retried after 2 / 10 /
//! 30 / 60 s with its reason shown; underruns and overflows are counted.
//! Exact pins come from a scratch model of the worker over lane 2's servo
//! and rubato models (`rust-workspace.md`, deriving pins).

use std::sync::Arc;

use super::fake::{FakeDevice, dvs};
use super::*;
use crate::playback::asio_format::unsupported_sample_text;
use crate::playback::asrc_servo::BASE_LATENCY_100NS;
use crate::playback::audio_out_block::ProgramBlock;
use crate::playback::program_max_worker::tests::wait_until;
use crate::playback::vban_out::tests::FakeClock;
use sp_core::audio_outputs::{AsioDest, OutputEntry};

const T0: i64 = 17_900_000_000_000_000;
const SLOT: i64 = 333_333;
const S: i64 = 10_000_000;
const DVS: &str = "Dante Virtual Soundcard (x64)";

fn entry() -> OutputEntry {
    OutputEntry::asio(
        "out-3",
        "DVS",
        AsioDest {
            driver: DVS.into(),
            channels: [0, 1],
        },
    )
}

fn out() -> AsioOut {
    AsioOut::for_entry(&entry()).unwrap()
}

fn block(k: i64) -> ProgramBlock {
    ProgramBlock {
        due_100ns: T0 + k * SLOT,
        samples: Some(vec![0.25; 3200].into()),
        substituted: false,
    }
}

/// Run `n` boundaries from `from` (each handled 5 ms late), the card draining
/// in between at 96 kHz / 128 frames (25 callbacks a boundary).
fn run(w: &mut AsioWorker, o: &AsioOut, d: &mut FakeDevice, from: i64, n: i64) {
    for k in from..from + n {
        w.step(o, d, T0 + k * SLOT + 50_000, Some(block(k)));
        d.drain(25);
    }
}

#[test]
fn it_opens_at_the_drivers_rate_and_runs_without_an_underrun() {
    let o = out();
    assert_eq!(o.snapshot().state, "opening");
    assert!(!o.is_running(), "no worker thread in a unit test");
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    assert_eq!(w.step(&o, &mut d, T0, None), POLL_100NS);
    assert_eq!(d.opened, vec![(DVS.to_string(), [0, 1])]);
    assert_eq!(d.starts, 1);
    let snap = o.snapshot();
    assert_eq!(snap.state, "running");
    assert_eq!(snap.reason, None);
    let s = &snap.status;
    assert_eq!(
        (
            s.driver.as_str(),
            s.channels,
            s.driver_rate,
            s.buffer_frames
        ),
        (DVS, [0, 1], 96_000, 128)
    );
    assert_eq!((s.out_channels, s.sample_type), (2, "Int32LSB"));
    assert_eq!((s.retry_in_s, s.reason_code), (None, None));
    run(&mut w, &o, &mut d, 1, 30 * 5);
    assert_eq!(d.underruns, 0);
    let snap = o.snapshot();
    assert_eq!(snap.blocks_sent, 150);
    assert_eq!(snap.blocks_dropped, 0);
    // The servo holds 66.625 ms (the ring + the splice's 5 ms + the 5 ms
    // hand-off), + the resampler's 256 and the driver's 128 frames = 4 ms.
    let s = &snap.status;
    assert!((s.latency_ms - 70.625).abs() < 1e-9, "{s:?}");
    assert!(s.ppm.abs() < 1.0, "{s:?}");
    assert_eq!(
        (s.underruns, s.overflows, s.resets, s.recentres),
        (0, 0, 0, 1)
    );
    // The ring after the card's last callbacks: 66.67 ms − 5 ms − 5 ms at
    // 96 kHz, less the first block's 4 frames (rubato's start index).
    assert_eq!(d.ring_frames(), 5_436);
    assert!(
        d.played.iter().any(|&x| (x - 0.25).abs() < 1e-3),
        "the program reached the card"
    );
}

#[test]
fn a_reset_request_closes_and_reopens_after_2_s() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    run(&mut w, &o, &mut d, 1, 30);
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    let now = T0 + 31 * SLOT;
    w.step(&o, &mut d, now, None);
    assert_eq!(d.closes, 1);
    let snap = o.snapshot();
    assert_eq!(
        (snap.state, snap.reason.as_ref().map(Reason::code)),
        ("waiting", Some("reset"))
    );
    assert_eq!(snap.status.resets, 1);
    assert_eq!(snap.status.reason_code, Some("reset"));
    assert_eq!(snap.status.retry_in_s, Some(2.0));
    assert_eq!(w.step(&o, &mut d, now + S, None), POLL_100NS);
    assert_eq!(o.snapshot().status.retry_in_s, Some(1.0), "counting down");
    assert_eq!(
        w.step(&o, &mut d, now + 2 * S - 5_000, None),
        5_000,
        "the loop waits only up to the retry"
    );
    assert_eq!(d.opened.len(), 1, "not before 2 s");
    w.step(&o, &mut d, now + 2 * S, None);
    assert_eq!(d.opened.len(), 2);
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.reason), ("running", None));
    assert_eq!(
        (snap.status.retry_in_s, snap.status.reason_code),
        (None, None)
    );
    assert_eq!(snap.status.resets, 1);
}

#[test]
fn a_busy_driver_retries_after_2_10_30_then_60_s() {
    let busy = || Err(Reason::Busy("init failed".into()));
    let o = out();
    let mut d = FakeDevice::answering(vec![
        busy(),
        busy(),
        busy(),
        busy(),
        busy(),
        Ok(dvs(96_000.0)),
    ]);
    let mut w = AsioWorker::new(T0);
    let mut attempts = Vec::new();
    let mut t = T0;
    while d.opened.len() < 6 && t < T0 + 300 * S {
        let before = d.opened.len();
        w.step(&o, &mut d, t, None);
        if d.opened.len() > before {
            attempts.push((t - T0) / S);
        }
        t += S / 10;
    }
    assert_eq!(attempts, vec![0, 2, 12, 42, 102, 162]);
    assert_eq!(d.closes, 5, "every failed open released the driver");
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.reason), ("running", None));
}

#[test]
fn a_busy_driver_shows_its_reason_and_the_retry() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Err(Reason::Busy("init failed".into()))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    let snap = o.snapshot();
    assert_eq!(snap.state, "waiting");
    assert_eq!(
        snap.reason.unwrap().text(),
        "the driver refused to start (in use by another program?): init failed"
    );
    assert_eq!(snap.status.retry_in_s, Some(2.0));
    assert_eq!(snap.status.reason_code, Some("busy"));
    assert_eq!(snap.status.resets, 0, "a failed open is no reset");
    assert_eq!(d.closes, 1, "a failed open releases the driver");
    assert_eq!(d.starts, 0);
}

#[test]
fn a_vanished_driver_stalls_and_closes_with_its_reason() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    run(&mut w, &o, &mut d, 1, 30);
    // no more callbacks: the card is gone
    let mut t = T0 + 31 * SLOT;
    while o.snapshot().state == "running" && t < T0 + 10 * S {
        w.step(&o, &mut d, t, None);
        t += SLOT;
    }
    let snap = o.snapshot();
    assert_eq!(snap.reason.as_ref().map(Reason::code), Some("stalled"));
    assert!(
        t - (T0 + 31 * SLOT) >= 2 * S,
        "not before 2 s without a callback"
    );
    assert_eq!((d.closes, snap.status.resets), (1, 1));
}

#[test]
fn a_rate_change_reopens_at_the_drivers_new_rate() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(48_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    run(&mut w, &o, &mut d, 1, 30);
    d.events.push_back(DeviceEvents {
        rate_changed: Some(48_000.0),
        ..Default::default()
    });
    let now = T0 + 31 * SLOT;
    w.step(&o, &mut d, now, None);
    assert_eq!(o.snapshot().reason, Some(Reason::RateChanged(48_000)));
    w.step(&o, &mut d, now + 2 * S, None);
    let snap = o.snapshot();
    assert_eq!((snap.state, snap.status.driver_rate), ("running", 48_000));
    // A new resampler at 48 kHz: 13 callbacks of 128 frames a boundary take
    // a little more than the 1 600 the program makes, and every one is full.
    let before = d.played.len();
    for k in 0..30 {
        let handled = now + 2 * S + k * SLOT;
        let b = ProgramBlock {
            due_100ns: handled - 50_000,
            samples: Some(vec![0.25; 3200].into()),
            substituted: false,
        };
        w.step(&o, &mut d, handled, Some(b));
        d.drain(13);
    }
    assert_eq!(d.played.len() - before, 2 * 30 * 13 * 128);
    assert_eq!(d.underruns, 0);
}

#[test]
fn a_run_of_60_s_resets_the_backoff() {
    let busy = || Err(Reason::Busy("init failed".into()));
    let o = out();
    let mut d = FakeDevice::answering(vec![busy(), busy()]);
    let mut w = AsioWorker::new(T0);
    let mut t = T0;
    while d.opened.len() < 3 {
        w.step(&o, &mut d, t, None);
        t += S / 10;
    }
    let opened_at = t - S / 10;
    assert_eq!(opened_at, T0 + 12 * S, "after 2 and 10 s");
    t = opened_at;
    while t < opened_at + 61 * S {
        t += S / 10;
        d.drain(1);
        w.step(&o, &mut d, t, None);
    }
    assert_eq!(o.snapshot().state, "running", "callbacks keep it open");
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    w.step(&o, &mut d, t, None);
    assert_eq!(
        o.snapshot().status.retry_in_s,
        Some(2.0),
        "after a 61 s run, not the 30 s two failed opens had reached"
    );
    t += 2 * S;
    w.step(&o, &mut d, t, None);
    assert_eq!(o.snapshot().state, "running");
    for _ in 0..10 {
        t += S / 10;
        d.drain(1);
        w.step(&o, &mut d, t, None);
    }
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    w.step(&o, &mut d, t, None);
    assert_eq!(
        o.snapshot().status.retry_in_s,
        Some(10.0),
        "a reset after a 1 s run is the second in a row"
    );
}

#[test]
fn a_closed_output_drops_the_blocks_it_is_handed() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Err(Reason::Busy("init failed".into()))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    w.step(&o, &mut d, T0 + SLOT, Some(block(1)));
    assert_eq!(o.snapshot().blocks_sent, 0);
    assert_eq!(d.opened.len(), 1, "a block does not open it early");
}

#[test]
fn the_first_block_primes_the_ring_and_underruns_count_across_reopens() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    d.drain(3);
    assert_eq!(
        d.underruns, 0,
        "an empty ring before the first block is no underrun"
    );
    w.step(&o, &mut d, T0 + SLOT + 50_000, Some(block(1)));
    assert_eq!(
        d.ring_frames(),
        8_636,
        "the start re-centre + the first block"
    );
    // 8 636 frames are 67 full callbacks; the 68th is short, then 132 dry.
    d.drain(200);
    w.step(&o, &mut d, T0 + SLOT + 60_000, None);
    assert_eq!(o.snapshot().status.underruns, 133);
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    w.step(&o, &mut d, T0 + SLOT + 70_000, None);
    w.step(&o, &mut d, T0 + SLOT + 70_000 + 2 * S, None);
    assert_eq!((d.starts, d.underruns), (2, 0), "a new run counts anew");
    w.step(&o, &mut d, T0 + SLOT + 80_000 + 2 * S, None);
    assert_eq!(
        o.snapshot().status.underruns,
        133,
        "the closed run's count stays"
    );
}

#[test]
fn a_block_stamped_far_ahead_overflows_the_ring_and_is_counted() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    // A block due 1 s after the worker's now (two clocks a second apart):
    // the start re-centre inserts 1.06 s, far more than the ring holds.
    let b = ProgramBlock {
        due_100ns: T0 + S,
        samples: None,
        substituted: false,
    };
    w.step(&o, &mut d, T0 + 50_000, Some(b));
    assert_eq!(d.ring_frames(), 22_413, "the ring is full: 19 200 + 3 213");
    assert_eq!(o.snapshot().status.overflows, 82_223);
}

#[test]
fn a_driver_rate_outside_8_to_384_khz_is_refused_and_released() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(1_000_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    let snap = o.snapshot();
    assert_eq!(
        snap.reason,
        Some(Reason::Refused("the driver reports 1000000 Hz".into()))
    );
    assert_eq!((d.starts, d.closes), (0, 1));
}

#[test]
fn an_unsupported_sample_type_waits_with_its_reason() {
    let o = out();
    let refused = Reason::Refused(unsupported_sample_text(20));
    let mut d = FakeDevice::answering(vec![Err(refused.clone())]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    assert_eq!(o.snapshot().reason, Some(refused));
}

#[test]
fn shutdown_closes_the_driver_once_and_nothing_reopens_it() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    w.shutdown(&mut d);
    assert_eq!(d.closes, 1);
    w.step(&o, &mut d, T0 + 3_600 * S, None);
    assert_eq!((d.opened.len(), d.closes), (1, 1));
}

#[test]
fn the_queue_drops_its_oldest_block_and_holds_the_delay() {
    let mut e = entry();
    e.delay_ms = 100;
    let o = AsioOut::for_entry(&e).unwrap();
    assert_eq!(o.target_100ns(), BASE_LATENCY_100NS + 1_000_000);
    assert_eq!(out().target_100ns(), BASE_LATENCY_100NS);
    for k in 0..30 {
        o.push(block(k));
    }
    assert_eq!(
        o.queued(),
        crate::playback::vban_out::queue_bound(1_000_000)
    );
    assert_eq!(o.queued(), 14);
    assert_eq!(o.snapshot().blocks_dropped, 16);
    o.discard();
    assert_eq!(o.queued(), 0);
    o.push(block(31));
    assert_eq!(o.queued(), 0, "a discarded output takes no block");
}

#[test]
fn only_an_asio_entry_makes_an_asio_output() {
    let vban = OutputEntry::vban(
        "out-1",
        "FOH",
        sp_core::audio_outputs::VbanDest {
            host: "h".into(),
            port: 6980,
            stream_name: "sp-program".into(),
            format: sp_core::audio_outputs::VbanSampleFormat::Int24,
        },
    );
    assert_eq!(
        AsioOut::for_entry(&vban).err().as_deref(),
        Some("not an ASIO entry")
    );
}

#[test]
fn off_windows_an_output_says_why_it_never_opens() {
    let o = out();
    o.set_windows_only();
    let snap = o.snapshot();
    assert_eq!(
        (snap.state, snap.reason, snap.status.reason_code),
        ("waiting", Some(Reason::WindowsOnly), Some("windows_only"))
    );
    assert_eq!(o.start_error(), None);
    o.set_start_error("spawning failed".into());
    assert_eq!(o.start_error().as_deref(), Some("spawning failed"));
}

#[test]
fn the_worker_loop_takes_blocks_until_stopped_then_releases_the_driver() {
    let o = Arc::new(out());
    let looping = o.clone();
    let thread = std::thread::Builder::new()
        .name("asio-test".into())
        .spawn(move || {
            let mut d = FakeDevice::answering(vec![]);
            let mut clock = FakeClock::at(T0);
            run_asio_worker(&looping, &mut d, &mut clock);
            d
        })
        .unwrap();
    wait_until("the worker runs and opened", || {
        o.is_running() && o.snapshot().state == "running"
    });
    o.push(block(1));
    wait_until("the block reached the ring", || {
        o.snapshot().blocks_sent == 1
    });
    o.stop();
    wait_until("the loop ended", || thread.is_finished());
    let d = thread.join().unwrap();
    assert!(!o.is_running());
    assert_eq!(
        (d.opened.len(), d.closes),
        (1, 1),
        "released once at the end"
    );
}

/// The installer ships THIRD-PARTY-NOTICES.txt; it must carry the MIT notice
/// of the rtrb this crate pins (re-copy it when the pin moves).
#[test]
fn the_installer_notice_carries_the_pinned_rtrbs_license() {
    const NOTICE: &str = include_str!("../../../../src-tauri/resources/THIRD-PARTY-NOTICES.txt");
    const MANIFEST: &str = include_str!("../../Cargo.toml");
    let notice = NOTICE.replace("\r\n", "\n");
    assert!(MANIFEST.contains("rtrb = \"=0.4.0\""), "the pin");
    assert!(
        notice.contains("rtrb 0.4.0"),
        "the notice names the pinned version"
    );
    assert!(notice.contains("Stjepan Glavina, Matthias Geier"));
    assert!(notice.contains(
        "The above copyright notice and this permission notice\n\
         shall be included in all copies or substantial portions\n\
         of the Software."
    ));
}

/// The ASIO host (Windows only) is azo 0.4.0 over azo-sys 0.3.2, without
/// its `host` feature (main-session ruling 10); the installer's notice
/// carries their MIT text (re-copy it when the pin moves).
#[test]
fn the_installer_notice_carries_the_pinned_azos_license() {
    const NOTICE: &str = include_str!("../../../../src-tauri/resources/THIRD-PARTY-NOTICES.txt");
    const MANIFEST: &str = include_str!("../../Cargo.toml");
    let notice = NOTICE.replace("\r\n", "\n");
    assert!(
        MANIFEST.contains("azo = { version = \"=0.4.0\", default-features = false }"),
        "the pin"
    );
    assert!(notice.contains("azo 0.4.0 and azo-sys 0.3.2"));
    assert!(notice.contains("Copyright (c) 2026 LastExceed"));
    assert!(notice.contains(
        "The above copyright notice and this permission notice shall be included in all\n\
         copies or substantial portions of the Software."
    ));
}

/// A driver reporting 48 000.4 Hz runs at the admitted 48 000; a sub-hertz
/// wobble of its later report (47 999.2) is no rate change.
#[test]
fn the_output_follows_the_admitted_whole_hertz_rate() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(48_000.4))]);
    let mut w = AsioWorker::new(T0);
    w.step(&o, &mut d, T0, None);
    assert_eq!(o.snapshot().status.driver_rate, 48_000);
    d.events.push_back(DeviceEvents {
        rate_changed: Some(47_999.2),
        ..Default::default()
    });
    d.drain(1);
    w.step(&o, &mut d, T0 + SLOT, None);
    assert_eq!((o.snapshot().state, d.closes), ("running", 0));
}

/// The status counts the closed runs' underruns and the running one's.
#[test]
fn the_underruns_of_a_closed_run_and_the_running_one_add_up() {
    let o = out();
    let mut d = FakeDevice::answering(vec![Ok(dvs(96_000.0)), Ok(dvs(96_000.0))]);
    let mut w = AsioWorker::new(T0);
    // One run: one block (8 636 frames), then 200 callbacks: 133 underruns.
    let one_dry_run = |w: &mut AsioWorker, d: &mut FakeDevice, at: i64| {
        w.step(&o, d, at, None);
        let b = ProgramBlock {
            due_100ns: at,
            samples: None,
            substituted: false,
        };
        w.step(&o, d, at + 50_000, Some(b));
        d.drain(200);
        w.step(&o, d, at + 60_000, None);
    };
    one_dry_run(&mut w, &mut d, T0);
    assert_eq!(o.snapshot().status.underruns, 133);
    d.events.push_back(DeviceEvents {
        reset: true,
        ..Default::default()
    });
    w.step(&o, &mut d, T0 + 70_000, None);
    one_dry_run(&mut w, &mut d, T0 + 70_000 + 2 * S);
    assert_eq!(d.starts, 2);
    assert_eq!(o.snapshot().status.underruns, 133 + 133);
}
