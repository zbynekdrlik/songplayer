//! Unit tests for [`super::LevelProbe`] (#184 round G4) — the 1 Hz window and
//! its reset are driven with synthetic `Instant`s, so they are deterministic.

use super::*;
use sp_core::audio_level::SILENCE_FLOOR_DBFS;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn assert_db(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() <= 1e-3,
        "expected {expected} dBFS, got {actual}"
    );
}

#[test]
fn window_stays_open_until_the_interval_elapses() {
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.add(&[1.0, -1.0]);
    assert_eq!(p.poll(t0), None);
    assert_eq!(p.poll(t0 + ms(999)), None);
    // The data added before the early polls is still pending.
    assert_eq!(p.pending().1, 2);
}

#[test]
fn window_closes_at_exactly_one_second_with_every_count() {
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.add(&[1.0, -1.0, 1.0, -1.0]);
    p.add(&[0.0, 0.0, 0.0, 0.0]);
    p.add_silence(96);
    p.add_silence(4);
    p.note_dropped();
    p.note_dropped();
    p.note_dropped();

    let r = p.poll(t0 + PROBE_INTERVAL).expect("window due at 1 s");
    // Σx² = 4 over 8 samples → mean square 0.5 → −3.01 dBFS.
    assert_db(r.rms_dbfs, -3.0103);
    assert_eq!(r.samples, 8);
    assert_eq!(r.blocks, 2);
    assert_eq!(r.silence_samples, 100);
    assert_eq!(r.dropped_blocks, 3);
    assert_eq!(r.window_ms, 1000);
}

#[test]
fn silence_never_enters_the_rms() {
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.add(&[0.5, -0.5]);
    p.add_silence(48_000);
    let r = p.poll(t0 + ms(1000)).unwrap();
    assert_db(r.rms_dbfs, -6.0206);
    assert_eq!(r.samples, 2);
}

#[test]
fn every_counter_resets_when_a_window_closes() {
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.add(&[1.0, -1.0]);
    p.add_silence(10);
    p.note_dropped();
    assert!(p.poll(t0 + ms(1200)).is_some());

    // The next window opens at the close time (1.2 s): nothing due before 2.2 s.
    assert_eq!(p.pending(), (SILENCE_FLOOR_DBFS, 0));
    assert_eq!(p.poll(t0 + ms(2199)), None);
    let r = p
        .poll(t0 + ms(2200))
        .expect("second window due 1 s after the first closed");
    assert_eq!(
        r,
        LevelReading {
            rms_dbfs: SILENCE_FLOOR_DBFS,
            samples: 0,
            blocks: 0,
            silence_samples: 0,
            dropped_blocks: 0,
            window_ms: 1000,
        }
    );
}

#[test]
fn a_late_poll_reports_the_real_window_length() {
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.add(&[0.25, -0.25]);
    let r = p.poll(t0 + ms(1750)).unwrap();
    assert_eq!(r.window_ms, 1750);
    assert_eq!(r.blocks, 1);
}

#[test]
fn pending_reads_the_open_window_without_closing_it() {
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.add(&[1.0, -1.0, 1.0, -1.0]);
    let (db, n) = p.pending();
    assert_db(db, 0.0);
    assert_eq!(n, 4);
    // pending() does not consume: the closed window still carries the samples.
    assert_eq!(p.poll(t0 + ms(1000)).unwrap().samples, 4);
}

#[test]
fn an_idle_overdue_window_restarts_at_the_first_block() {
    // Nothing measured for 5 s (the tap had no viewer): the first block re-opens
    // the window, so the next line covers 1 s of measurement, not the 5 s gap.
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.restart_if_idle(t0 + ms(5000));
    p.add(&[1.0, -1.0]);
    assert_eq!(p.poll(t0 + ms(5999)), None);
    let r = p.poll(t0 + ms(6000)).expect("due 1 s after the restart");
    assert_eq!((r.window_ms, r.samples, r.blocks), (1000, 2, 1));
}

#[test]
fn an_idle_window_restarts_at_exactly_one_second() {
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.restart_if_idle(t0 + PROBE_INTERVAL);
    assert_eq!(p.poll(t0 + ms(1999)), None);
    assert_eq!(p.poll(t0 + ms(2000)).unwrap().window_ms, 1000);
}

#[test]
fn an_idle_window_that_is_not_yet_due_is_kept() {
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.restart_if_idle(t0 + ms(500));
    p.add(&[1.0, -1.0]);
    assert_eq!(p.poll(t0 + ms(1000)).unwrap().window_ms, 1000);
}

#[test]
fn a_window_with_data_is_never_restarted() {
    // Overdue but NOT idle: the data already in the window must be reported.
    let t0 = Instant::now();
    let mut p = LevelProbe::new(t0);
    p.add(&[1.0, -1.0]);
    p.restart_if_idle(t0 + ms(1200));
    let r = p
        .poll(t0 + ms(1200))
        .expect("the overdue window with data closes");
    assert_eq!((r.window_ms, r.samples), (1200, 2));
}
