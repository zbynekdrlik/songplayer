//! The wall's refresh grid (`vblank.rs`, #223 follow-up): the output pick,
//! the boot, the refresh count and the fit, on made-up wake-up instants.
//! The exact pins come from a scratch model of the fit (plain left folds,
//! like the Rust).
//! Wired via `#[cfg(test)] #[path = "vblank_tests.rs"] mod tests;`.

use std::time::{Duration, Instant};

use super::{OutputInfo, Seen, VBLANK_STALE, VblankFit, VblankGrid, grid_is_fresh, pick_output};

/// The SNV wall's measured period, ns (59.99988 Hz).
const P: u64 = 16_666_700;

fn ns(n: u64) -> Duration {
    Duration::from_nanos(n)
}

fn output(name: &str, left: i32, top: i32, width: u32, height: u32) -> OutputInfo {
    OutputInfo {
        name: name.to_string(),
        left,
        top,
        width,
        height,
        attached: true,
    }
}

/// A fit fed wake-ups at `base + t` for each `t` (ns), and what each was.
fn fed(base: Instant, times: impl IntoIterator<Item = u64>) -> (VblankFit, Vec<Seen>) {
    let mut fit = VblankFit::default();
    let seen = times
        .into_iter()
        .map(|t| fit.observe(base + ns(t)))
        .collect();
    (fit, seen)
}

fn grid(base: Instant, at: u64, period: u64) -> Option<VblankGrid> {
    Some(VblankGrid {
        at: base + ns(at),
        period: ns(period),
    })
}

/// SNV, measured 9.10.2026 (`phase_lock.py`): DWM composes the wall's
/// output on the PRIMARY display's refresh — its composed frames sit 2.3 ms
/// after DISPLAY5's vblank for 40 s, while the wall's own vblank (DISPLAY1,
/// 59.979 Hz) drifts through them — and Arena renders in DWM's rhythm. So
/// the primary desktop is picked, in either DXGI order, never the wall.
#[test]
fn the_primary_desktop_is_picked_its_refresh_is_the_composition_clock() {
    let primary = output(r"\\.\DISPLAY5", 0, 0, 3840, 2160);
    let wall = output(r"\\.\DISPLAY1", 3840, 0, 7680, 1080);
    assert_eq!(pick_output(&[primary.clone(), wall.clone()]), Some(0));
    assert_eq!(pick_output(&[wall, primary]), Some(1));
}

/// Only (0, 0) is the primary: a display below it or beside it is not.
#[test]
fn only_the_desktop_origin_is_the_primary() {
    assert!(output("p", 0, 0, 1920, 1080).is_primary());
    assert!(!output("below", 0, 1080, 1920, 1080).is_primary());
    assert!(!output("beside", 1920, 0, 1920, 1080).is_primary());
}

/// No attached display at the desktop origin (a detached one, or only
/// second displays): no output, never a second display's refresh.
#[test]
fn without_an_attached_primary_no_output_is_picked() {
    let mut off = output("p", 0, 0, 1920, 1080);
    off.attached = false;
    let wall = output("wall", 1920, 0, 7680, 1080);
    assert_eq!(pick_output(&[off, wall.clone()]), None);
    assert_eq!(pick_output(&[wall]), None);
    assert_eq!(pick_output(&[]), None);
}

#[test]
fn the_label_names_the_device_and_its_size() {
    let wall = output(r"\\.\DISPLAY2", 3840, 0, 7680, 1080);
    assert_eq!(wall.label(), r"\\.\DISPLAY2 7680x1080");
}

#[test]
fn a_grid_is_fresh_up_to_the_stale_bound() {
    let seen = Instant::now();
    assert!(grid_is_fresh(seen, seen));
    assert!(grid_is_fresh(seen, seen + VBLANK_STALE));
    assert!(!grid_is_fresh(seen, seen + VBLANK_STALE + ns(1)));
}

/// Sixteen wake-ups (fifteen intervals) start the count; the grid is
/// reported once sixty refreshes are counted, at the last one.
#[test]
fn the_grid_is_reported_from_the_sixtieth_counted_refresh() {
    let base = Instant::now();
    let (fit, seen) = fed(base, (0..74).map(|k| k * P));
    assert!(seen[..16].iter().all(|s| *s == Seen::Booting), "{seen:?}");
    assert!(seen[16..].iter().all(|s| *s == Seen::Counted), "{seen:?}");
    assert_eq!(fit.grid(), None, "59 counted");

    let (fit, _) = fed(base, (0..75).map(|k| k * P));
    assert_eq!(fit.grid(), grid(base, 74 * P, P));
    assert_eq!((fit.missed(), fit.early()), (0, 0));
}

/// A wake-up's latency averages out: ±300 µs on every wake-up moves the
/// fitted refresh by under 10 µs and its period by under 100 ns (model:
/// 3.7 µs and 31 ns).
#[test]
fn wakeup_jitter_averages_out_of_the_grid() {
    let base = Instant::now();
    let jitter = |k: u64| {
        if k.is_multiple_of(2) {
            k * P + 300_000
        } else {
            k * P - 300_000
        }
    };
    let (fit, _) = fed(base, (0..400).map(jitter));
    let got = fit.grid().expect("a grid");
    let true_at = base + ns(399 * P);
    let at_error = if got.at > true_at {
        got.at - true_at
    } else {
        true_at - got.at
    };
    assert!(at_error < Duration::from_micros(10), "{at_error:?}");
    let period_error = got.period.as_nanos().abs_diff(u128::from(P));
    assert!(period_error < 100, "{period_error} ns");
}

/// A refresh the waits did not see is counted (the gap is two periods),
/// and the grid stays on it.
#[test]
fn a_missed_refresh_is_counted_and_the_grid_stays_exact() {
    let base = Instant::now();
    let (mut fit, _) = fed(base, (0..75).map(|k| k * P));
    assert_eq!(fit.observe(base + ns(76 * P)), Seen::Counted);
    assert_eq!(fit.missed(), 1);
    assert_eq!(fit.grid(), grid(base, 76 * P, P));
}

/// A wait that returned within half a period of a refresh is not a new
/// one: left out, counted as early; the next real refresh counts as usual.
#[test]
fn a_wakeup_within_half_a_period_is_early_and_left_out() {
    let base = Instant::now();
    let (mut fit, _) = fed(base, (0..75).map(|k| k * P));
    assert_eq!(fit.observe(base + ns(74 * P + 1_000_000)), Seen::Early);
    assert_eq!(fit.observe(base + ns(74 * P + 2_000_000)), Seen::Early);
    assert_eq!(fit.early(), 2);
    assert_eq!(fit.grid(), grid(base, 74 * P, P));
    assert_eq!(fit.observe(base + ns(75 * P)), Seen::Counted);
    assert_eq!(fit.grid(), grid(base, 75 * P, P));
    assert_eq!(fit.missed(), 0);
}

/// From the sixtieth counted refresh the count uses the fitted period, not
/// the boot's: booted on 16.5 ms, then the display's P, a gap of 120
/// refreshes is 120 (the boot period would count 121).
#[test]
fn the_count_uses_the_fit_from_the_sixtieth_refresh() {
    let base = Instant::now();
    const BOOT: u64 = 16_500_000;
    let t15 = 15 * BOOT;
    let times = (0..16)
        .map(|k| k * BOOT)
        .chain((1..60).map(|k| t15 + k * P));
    let (mut fit, _) = fed(base, times);
    assert_eq!(fit.grid(), grid(base, t15 + 59 * P, P));
    assert_eq!(fit.observe(base + ns(t15 + 179 * P)), Seen::Counted);
    assert_eq!(fit.missed(), 119);
    assert_eq!(fit.grid(), grid(base, t15 + 179 * P, P));
}

/// The fit covers the last 240 refreshes: after a switch from P to 50 Hz,
/// two old refreshes still bend it at 238 new ones, none at 239.
#[test]
fn the_fit_forgets_refreshes_older_than_the_window() {
    let base = Instant::now();
    const P50: u64 = 20_000_000;
    let last_old = 315 * P;
    let (mut fit, _) = fed(base, (0..316).map(|k| k * P));
    for j in 1..239 {
        fit.observe(base + ns(last_old + j * P50));
    }
    let bent = fit.grid().expect("a grid");
    assert_ne!(bent.period, ns(P50), "two old refreshes are in the fit");
    fit.observe(base + ns(last_old + 239 * P50));
    assert_eq!(fit.grid(), grid(base, last_old + 239 * P50, P50));
}

/// A display refreshes between 4 ms and 50 ms, both included: waits that
/// return faster or slower measure no display, and report no grid.
#[test]
fn only_a_display_period_gives_a_grid() {
    let base = Instant::now();
    for period in [4_000_000, 50_000_000] {
        let (fit, _) = fed(base, (0..75).map(|k| k * period));
        assert_eq!(fit.grid(), grid(base, 74 * period, period), "{period} ns");
    }
    for period in [3_999_999, 50_000_001, 1_000_000] {
        let (fit, _) = fed(base, (0..75).map(|k| k * period));
        assert_eq!(fit.grid(), None, "{period} ns");
    }
}

/// The boot takes the MEDIAN interval: two spurious wake-ups 0.5 ms after a
/// refresh among the first fifteen intervals do not decide the period.
#[test]
fn a_boot_with_two_spurious_wakeups_takes_the_displays_period() {
    let base = Instant::now();
    let times = [0, 500_000, P, 2 * P, 2 * P + 500_000]
        .into_iter()
        .chain((3..73).map(|k| k * P));
    let (fit, seen) = fed(base, times);
    assert_eq!(seen.iter().filter(|s| **s == Seen::Booting).count(), 16);
    assert_eq!(seen.iter().filter(|s| **s == Seen::Counted).count(), 59);
    assert_eq!(fit.grid(), grid(base, 72 * P, P));
}
