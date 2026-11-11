//! #229 (the owner's ruling, 9.11.2026): a card that takes audio more than
//! 1 % off the rate its driver reports is no usable clock — the pure rate
//! watch, its window and its verdicts, both sides of every edge.

use super::*;

const T: i64 = 17_900_000_000_000_000;
/// DVS as SNV runs it: 96 kHz, 128-frame buffers (a 10 s window).
const DVS: (f64, u32) = (96_000.0, 128);

#[test]
fn the_rate_watch_constants() {
    assert_eq!(
        (
            RATE_WINDOW_100NS,
            RATE_WINDOW_BUFFERS,
            OFF_RATE_PPM,
            OFF_RATE_STRIKES_PLAYING
        ),
        (100_000_000, 500, 10_000.0, 2)
    );
}

/// At least 10 s, and at least 500 of the driver's buffers: PP's SoundGrid
/// (1024 frames at 44.1 kHz) needs 11.6 s.
#[test]
fn a_window_is_10_s_or_500_buffers_whichever_is_longer() {
    assert_eq!(rate_window_100ns(128, 96_000.0), 100_000_000);
    assert_eq!(rate_window_100ns(1024, 44_100.0), 116_099_773);
    assert_eq!(rate_window_100ns(1024, 96_000.0), 100_000_000, "5.3 s");
    assert_eq!(rate_window_100ns(2048, 96_000.0), 106_666_667);
}

#[test]
fn the_consumed_rate_is_measured_in_ppm_of_the_drivers_rate() {
    let close = |got: f64, want: f64| assert!((got - want).abs() < 1e-6, "{got} vs {want}");
    close(consumed_ppm(960_000, 100_000_000, 96_000.0), 0.0);
    close(consumed_ppm(987_840, 100_000_000, 96_000.0), 29_000.0);
    close(consumed_ppm(932_160, 100_000_000, 96_000.0), -29_000.0);
    close(consumed_ppm(1_920_000, 200_000_000, 96_000.0), 0.0);
    close(consumed_ppm(453_789, 100_000_000, 44_100.0), 29_000.0);
}

/// A window closes at its length (never a step earlier) and reads within
/// 1 % as plausible; the next window runs from that close.
#[test]
fn a_window_closes_at_its_length_and_the_next_runs_from_there() {
    let mut w = RateWatch::default();
    let window = rate_window_100ns(DVS.1, DVS.0);
    assert_eq!(w.step(1_000, T, DVS, 1), RateStep::Measuring, "the start");
    assert_eq!(
        w.step(1_000 + 2_000_000, T + window - 1, DVS, 1),
        RateStep::Measuring,
        "a step before the window's end"
    );
    assert_eq!(
        w.step(1_000 + 960_000, T + window, DVS, 1),
        RateStep::Plausible
    );
    // From that close: 1 % + 1 frame more over the next window.
    assert!(matches!(
        w.step(1_000 + 960_000 + 969_601, T + 2 * window, DVS, 1),
        RateStep::OffRate { .. }
    ));
}

/// 1 % off is the bound: 9 600 frames over a 96 kHz window. A frame inside
/// it is plausible, a frame beyond it is not, on either side.
#[test]
fn one_percent_is_the_bound_on_either_side() {
    let window = rate_window_100ns(DVS.1, DVS.0);
    let verdict = |frames: u64| {
        let mut w = RateWatch::default();
        w.step(0, T, DVS, 1);
        w.step(frames, T + window, DVS, 1)
    };
    assert_eq!(verdict(969_599), RateStep::Plausible);
    assert_eq!(verdict(950_401), RateStep::Plausible);
    let RateStep::OffRate { ppm } = verdict(969_601) else {
        panic!("1 % + 1 frame more")
    };
    assert!((ppm - 10_001.041_666).abs() < 1e-3, "{ppm}");
    let RateStep::OffRate { ppm } = verdict(950_399) else {
        panic!("1 % + 1 frame less")
    };
    assert!((ppm + 10_001.041_666).abs() < 1e-3, "{ppm}");
}

/// Two strikes: one off window is not enough, a plausible one in between
/// starts the count again, two in a row are the verdict.
#[test]
fn a_playing_output_needs_two_off_windows_in_a_row() {
    let window = rate_window_100ns(DVS.1, DVS.0);
    let (on, off) = (960_000, 987_840);
    let mut w = RateWatch::default();
    let mut frames = 0;
    let mut at = T;
    let mut next = |w: &mut RateWatch, taken: u64| {
        frames += taken;
        at += window;
        w.step(frames, at, DVS, OFF_RATE_STRIKES_PLAYING)
    };
    assert_eq!(w.step(0, T, DVS, 2), RateStep::Measuring);
    assert_eq!(next(&mut w, off), RateStep::Measuring, "one off window");
    assert_eq!(
        next(&mut w, on),
        RateStep::Plausible,
        "the count starts again"
    );
    assert_eq!(
        next(&mut w, off),
        RateStep::Measuring,
        "one off window again"
    );
    let RateStep::OffRate { ppm } = next(&mut w, off) else {
        panic!("two off windows in a row")
    };
    assert!((ppm - 29_000.0).abs() < 1e-6, "{ppm}");
    let RateStep::OffRate { .. } = next(&mut w, off) else {
        panic!("a third")
    };
}

/// A clock read that goes back starts the window again from it (never a
/// negative span, never a window that never closes).
#[test]
fn a_clock_that_goes_back_starts_the_window_again() {
    let window = rate_window_100ns(DVS.1, DVS.0);
    let mut w = RateWatch::default();
    w.step(0, T, DVS, 1);
    assert_eq!(w.step(5_000, T - 1, DVS, 1), RateStep::Measuring);
    assert_eq!(
        w.step(5_000 + 960_000, T - 1 + window - 1, DVS, 1),
        RateStep::Measuring,
        "the window runs from the read that went back"
    );
    assert_eq!(
        w.step(5_000 + 960_000, T - 1 + window, DVS, 1),
        RateStep::Plausible
    );
}

/// A read at the window's start instant moves nothing: the window keeps
/// its first frame count (only a read that goes back starts it again).
#[test]
fn a_read_at_the_windows_start_instant_keeps_the_window() {
    let window = rate_window_100ns(DVS.1, DVS.0);
    let mut w = RateWatch::default();
    w.step(0, T, DVS, 1);
    assert_eq!(w.step(100_000, T, DVS, 1), RateStep::Measuring);
    assert_eq!(
        w.step(969_000, T + window, DVS, 1),
        RateStep::Plausible,
        "969 000 frames over the window from 0: +0.94 % (from 100 000: −9.5 %)"
    );
}

/// A run's windows start after a quarter second of the card's audio: at a
/// quarter second exactly, never a frame before.
#[test]
fn the_windows_start_after_a_quarter_second_of_ticks() {
    assert_eq!(RATE_SETTLE_PER_S, 4.0);
    assert!(rate_settled(12_000, 48_000.0));
    assert!(!rate_settled(11_999, 48_000.0));
    assert!(rate_settled(11_025, 44_100.0));
    assert!(!rate_settled(11_024, 44_100.0));
}

#[test]
fn an_off_rate_card_has_its_own_code() {
    assert_eq!(
        (
            Reason::ClockOffRate.code(),
            Reason::ClockOffRate.text().as_str()
        ),
        (
            "clock_off_rate",
            "the card takes audio more than 1 % off the rate its driver reports: no usable clock (e.g. Waves SoundGrid with its network off)"
        )
    );
    assert_eq!(
        sp_core::audio_outputs::asio_reason_sk(Reason::ClockOffRate.code()),
        "karta neberie zvuk svojou frekvenciou — hodiny sa nedajú použiť (napr. SoundGrid bez siete)"
    );
    assert_eq!(
        sp_core::dante_status::dante_reason_sk(Reason::ClockOffRate.code()),
        "zariadenie Dante neberie zvuk svojou frekvenciou — hodiny sa nedajú použiť"
    );
}

/// Review round 2: the longest callback gap the windows read through is
/// 100 ms, or three of the driver's buffers when that is longer.
#[test]
fn a_callback_gap_over_100_ms_or_3_buffers_is_a_stall() {
    assert_eq!((RATE_GAP_MIN_100NS, RATE_GAP_PERIODS), (1_000_000, 3));
    assert_eq!(rate_gap_limit_100ns(128, 96_000.0), 1_000_000);
    assert_eq!(rate_gap_limit_100ns(1024, 44_100.0), 1_000_000, "69.7 ms");
    assert_eq!(rate_gap_limit_100ns(2048, 44_100.0), 1_393_197);
}

/// A DVS card read the way the worker steps it: its count tracked and the
/// watch stepped every 50 ms.
struct Feed {
    w: RateWatch,
    frames: u64,
    at: i64,
    strikes: u32,
}

/// A read: 50 ms.
const READ: i64 = 500_000;

impl Feed {
    fn new(strikes: u32) -> Self {
        let mut f = Self {
            w: RateWatch::default(),
            frames: 0,
            at: T,
            strikes,
        };
        f.read();
        f
    }

    fn read(&mut self) -> RateStep {
        self.w.track(self.frames, self.at, DVS);
        self.w.step(self.frames, self.at, DVS, self.strikes)
    }

    /// `reads` reads, the card taking `per_read` frames between two (0: a
    /// stall); the first verdict and when.
    fn run(&mut self, reads: u32, per_read: u64) -> Option<(i64, RateStep)> {
        let mut first = None;
        for _ in 0..reads {
            self.at += READ;
            self.frames += per_read;
            let step = self.read();
            if first.is_none() && step != RateStep::Measuring {
                first = Some((self.at, step));
            }
        }
        first
    }

    /// The card's callbacks stop for `gap`, then it moves again (with the
    /// frames it owes, so only the gap differs).
    fn gap(&mut self, gap: i64) {
        self.at += gap;
        self.frames += (gap as u64) * 96_000 / 10_000_000;
        self.read();
    }
}

/// On its rate: 4 800 frames a read; 2.9 % more: 4 939.2, as 4 939.
const ON: u64 = 4_800;
const OFF: u64 = 4_939;

/// A gap up to the limit is read through: the window closes at its length
/// from the start (T + 10 s). A longer one starts the window again from
/// the card's next move (T + 1.1000001 s), so it closes 10 s after that.
#[test]
fn a_callback_gap_over_the_limit_starts_the_window_again() {
    let window = rate_window_100ns(DVS.1, DVS.0);
    let mut f = Feed::new(1);
    f.run(20, ON);
    f.gap(1_000_000);
    let verdict = f.run(400, ON).unwrap();
    assert_eq!(verdict, (T + window, RateStep::Plausible));
    let mut f = Feed::new(1);
    f.run(20, ON);
    f.gap(1_000_001);
    let resumed = T + 10_000_000 + 1_000_001;
    assert_eq!(
        f.run(400, ON).unwrap(),
        (resumed + window, RateStep::Plausible)
    );
}

/// A gap also ends the strikes so far: an off window, a stall, then one
/// more off window is one strike, never the verdict; the next is.
#[test]
fn a_callback_gap_ends_the_strikes_so_far() {
    let window = rate_window_100ns(DVS.1, DVS.0);
    let mut f = Feed::new(OFF_RATE_STRIKES_PLAYING);
    assert_eq!(f.run(200, OFF), None, "one strike at T + 10 s");
    f.gap(20_000_000);
    let resumed = T + window + 20_000_000;
    let (at, step) = f.run(500, OFF).unwrap();
    assert_eq!(at, resumed + 2 * window, "two windows after the gap");
    assert!(matches!(step, RateStep::OffRate { .. }), "{step:?}");
}

/// While a stall lasts it cannot be told from a slow card: a window that
/// closes in it is judged (a silent proof's one strike); only the card's
/// next move starts the windows again.
#[test]
fn a_window_that_closes_inside_a_stall_is_judged() {
    let window = rate_window_100ns(DVS.1, DVS.0);
    let mut f = Feed::new(1);
    assert_eq!(f.run(190, ON), None, "9.5 s on its rate");
    let (at, step) = f.run(40, 0).unwrap();
    assert_eq!(at, T + window);
    assert!(matches!(step, RateStep::OffRate { .. }), "{step:?}");
}
