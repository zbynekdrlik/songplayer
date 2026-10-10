//! The refresh MAX is paced on: the primary display's vertical blank
//! (#223 follow-up, 9.10.2026).
//!
//! Resolume Arena renders in the rhythm of the desktop compositor (DWM) and
//! takes whatever Spout's shared texture holds at its own instant in each
//! frame. A 30 fps `SP-program-MAX` paced on SongPlayer's genlock grid drifts
//! against that rhythm (a few ppm), so for minutes at a time its sends land
//! next to Arena's instant and some pictures show for one frame instead of
//! two: the wall stutters. An ordinary Spout sender renders in the
//! display's rhythm; MAX does the same by sending at a fixed point of it.
//!
//! WHICH refresh, measured on SNV (`ops/phase_lock.py`): DWM composes the
//! wall's output at 60.0000 Hz, 2.3 ms after the PRIMARY display's vblank
//! (DISPLAY5, 60.0000 Hz) for 40 s, while the wall's own vblank (DISPLAY1,
//! 59.979 Hz: the LED processor's timing) drifts through the composed
//! frames. DWM drives every output from the primary's refresh; pacing on the
//! wall's own vblank re-picked a slot every ~50 s.
//!
//! - [`pick_output`]: the attached output at the desktop origin (the
//!   primary), on whichever adapter drives it.
//! - [`VblankFit`]: the instants a thread wakes from DXGI's `WaitForVBlank`
//!   on it, counted into a refresh index and fitted by least squares over
//!   the last [`VBLANK_WINDOW`] refreshes, so a wake-up's latency (tens of
//!   µs, at times a ms) averages out: [`VblankGrid`] = one refresh instant
//!   and the measured period.
//!
//! Pure and Linux-tested; the thread that waits is `win/vblank.rs`
//! (`VblankTracker`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Intervals between the first wake-ups whose median is the first period
/// estimate (a missed refresh in them does not move a median).
pub const VBLANK_BOOT_INTERVALS: usize = 15;

/// Refreshes in the fit: 4 s at 60 Hz.
pub const VBLANK_WINDOW: usize = 240;

/// Refreshes counted before a grid is reported: 1 s at 60 Hz.
pub const VBLANK_MIN_FIT: usize = 60;

/// The periods a display refresh can have (250 Hz … 20 Hz); outside them
/// the waits do not measure a display.
pub const VBLANK_MIN_PERIOD: Duration = Duration::from_millis(4);
pub const VBLANK_MAX_PERIOD: Duration = Duration::from_millis(50);

/// A grid older than this (no wake-up counted since) is not used: the
/// waiting thread stalled or its output went away.
pub const VBLANK_STALE: Duration = Duration::from_millis(100);

/// #243: a `WaitForVBlank` that returned sooner did not wait — far below
/// any display's period ([`VBLANK_MIN_PERIOD`]). PP, 9.10.2026: with the
/// laptop panel dark (still attached), every wait returned at once.
pub const VBLANK_MIN_WAIT: Duration = Duration::from_millis(1);

/// The longest sleep after a wait that did not wait ([`not_waiting_sleep`]).
pub const VBLANK_IDLE_MAX: Duration = Duration::from_millis(250);

/// No wait blocked for this long: the output does not tick
/// ([`vblank_state`]).
pub const VBLANK_NOT_TICKING: Duration = Duration::from_secs(2);

/// A refresh this long after the last counted one boots the fit afresh
/// ([`VblankFit::observe`]): the refresh index is never carried across a
/// long gap, where its rounding could be off by one.
pub const VBLANK_RESTART: Duration = Duration::from_secs(1);

/// Whether a `WaitForVBlank` that took `wait` waited for a refresh
/// ([`VBLANK_MIN_WAIT`]). A wake-up that did not wait is never fed to the
/// fit.
pub fn waited(wait: Duration) -> bool {
    wait >= VBLANK_MIN_WAIT
}

/// The sleep after the `streak`-th wait in a row that did not wait: 1 ms
/// doubling to [`VBLANK_IDLE_MAX`], never none — a dark output never spins
/// the time-critical thread (#243), and is picked up within
/// [`VBLANK_IDLE_MAX`] once it presents.
pub fn not_waiting_sleep(streak: u32) -> Duration {
    let shift = streak.saturating_sub(1).min(8);
    Duration::from_millis(1u64 << shift).min(VBLANK_IDLE_MAX)
}

/// The paced output's state (#243): `GET /api/v1/program` `max.vblank_state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VblankState {
    /// No grid yet, but a wait blocked within [`VBLANK_NOT_TICKING`] (or the
    /// tracker started within it): the fit is booting.
    Measuring,
    /// A fresh grid: the sends go on the output's refresh.
    Ticking,
    /// No wait blocked for [`VBLANK_NOT_TICKING`]: the output does not
    /// present (dark, off), or its waits fail. The sends go at the constant
    /// lead.
    NotTicking,
}

impl VblankState {
    /// The API's word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measuring => "measuring",
            Self::Ticking => "ticking",
            Self::NotTicking => "not_ticking",
        }
    }
}

/// The state at `now`: [`VblankState::Ticking`] with a fresh grid, else
/// measuring until [`VBLANK_NOT_TICKING`] after the last wait that blocked
/// (`waited_at`; before any, the tracker's start), then not ticking.
pub fn vblank_state(
    grid_fresh: bool,
    waited_at: Option<Instant>,
    started: Instant,
    now: Instant,
) -> VblankState {
    if grid_fresh {
        return VblankState::Ticking;
    }
    let since = waited_at.unwrap_or(started);
    if now.saturating_duration_since(since) > VBLANK_NOT_TICKING {
        VblankState::NotTicking
    } else {
        VblankState::Measuring
    }
}

/// One display output, as DXGI describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInfo {
    /// The GDI device name (`\\.\DISPLAY2`).
    pub name: String,
    /// The desktop rectangle's top-left corner and size.
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// Part of the desktop (a detached output shows nothing).
    pub attached: bool,
}

impl OutputInfo {
    /// The primary display: its desktop rectangle starts at (0, 0).
    pub fn is_primary(&self) -> bool {
        self.left == 0 && self.top == 0
    }

    /// `\\.\DISPLAY5 3840x2160`: the telemetry's `vblank_output`.
    pub fn label(&self) -> String {
        format!("{} {}x{}", self.name, self.width, self.height)
    }
}

/// The output whose refresh paces MAX, by its index in `outputs`: the
/// attached primary display, DWM's clock (the module doc). `None` when no
/// attached output sits at the desktop origin: never a second display's
/// own refresh.
pub fn pick_output(outputs: &[OutputInfo]) -> Option<usize> {
    outputs
        .iter()
        .position(|output| output.attached && output.is_primary())
}

/// A display's refresh: `at` is a vertical blank, and every `at + k·period`
/// (any integer k) is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VblankGrid {
    pub at: Instant,
    pub period: Duration,
}

/// Whether a grid last counted at `seen` may still be used at `now`
/// ([`VBLANK_STALE`]).
pub fn grid_is_fresh(seen: Instant, now: Instant) -> bool {
    now.saturating_duration_since(seen) <= VBLANK_STALE
}

/// What one wake-up was ([`VblankFit::observe`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Counted toward the first period estimate.
    Booting,
    /// Less than half a period after the last counted one: not a new
    /// refresh, left out (a wait that returned at once).
    Early,
    /// Counted as a refresh.
    Counted,
}

/// The refresh grid fitted from `WaitForVBlank` wake-ups. Times are ns since
/// the first wake-up, as `f64` (exact to well under a ns for weeks).
#[derive(Debug, Default)]
pub struct VblankFit {
    base: Option<Instant>,
    /// The last counted wake-up: its refresh index and time.
    last: Option<(i64, f64)>,
    boot: Vec<f64>,
    /// The first period estimate (the boot median).
    period0: Option<f64>,
    /// The counted refreshes in the fit, oldest first.
    samples: VecDeque<(i64, f64)>,
    early: u64,
    missed: u64,
}

impl VblankFit {
    /// Count a wake-up at `at` (monotonic: never before the last one). One
    /// more than [`VBLANK_RESTART`] after the last counted refresh boots
    /// the fit afresh (the counters kept).
    pub fn observe(&mut self, at: Instant) -> Seen {
        if let (Some(base), Some((_, last_t))) = (self.base, self.last) {
            let t = at.saturating_duration_since(base).as_nanos() as f64;
            if t - last_t > VBLANK_RESTART.as_nanos() as f64 {
                *self = Self {
                    early: self.early,
                    missed: self.missed,
                    ..Self::default()
                };
            }
        }
        let base = *self.base.get_or_insert(at);
        let t = at.saturating_duration_since(base).as_nanos() as f64;
        let (Some(period), Some((index, last_t))) = (self.period(), self.last) else {
            return self.boot_with(t);
        };
        let refreshes = ((t - last_t) / period).round();
        if refreshes < 1.0 {
            self.early += 1;
            return Seen::Early;
        }
        let refreshes = refreshes as i64;
        self.missed += (refreshes - 1) as u64;
        let index = index + refreshes;
        self.last = Some((index, t));
        self.samples.push_back((index, t));
        if self.samples.len() > VBLANK_WINDOW {
            self.samples.pop_front();
        }
        Seen::Counted
    }

    /// The period the next wake-up is counted with: the fit's once
    /// [`VBLANK_MIN_FIT`] refreshes are in it, else the boot median.
    fn period(&self) -> Option<f64> {
        self.fitted().map(|(_, period)| period).or(self.period0)
    }

    /// The fit once [`VBLANK_MIN_FIT`] refreshes are counted: the fitted
    /// time of the last one, and the period.
    fn fitted(&self) -> Option<(f64, f64)> {
        (self.samples.len() >= VBLANK_MIN_FIT).then(|| fit(&self.samples))
    }

    /// A wake-up before the first period estimate: collect its interval;
    /// the median of [`VBLANK_BOOT_INTERVALS`] starts the count, unless it
    /// is no display period (then the next ones are collected anew).
    fn boot_with(&mut self, t: f64) -> Seen {
        if let Some((_, last_t)) = self.last {
            self.boot.push(t - last_t);
        }
        self.last = Some((0, t));
        if self.boot.len() == VBLANK_BOOT_INTERVALS {
            let mut intervals = std::mem::take(&mut self.boot);
            intervals.sort_by(f64::total_cmp);
            let median = intervals[intervals.len() / 2];
            if plausible(median) {
                self.period0 = Some(median);
                self.samples.push_back((0, t));
            }
        }
        Seen::Booting
    }

    /// The fitted grid once [`VBLANK_MIN_FIT`] refreshes are counted and the
    /// period is a display's ([`VBLANK_MIN_PERIOD`] … [`VBLANK_MAX_PERIOD`]).
    pub fn grid(&self) -> Option<VblankGrid> {
        let (at, period) = self.fitted()?;
        if !plausible(period) {
            return None;
        }
        Some(VblankGrid {
            at: self.base? + Duration::from_nanos(at.max(0.0).round() as u64),
            period: Duration::from_nanos(period.round() as u64),
        })
    }

    /// Refreshes the waits did not see (a gap of n periods counts n − 1).
    pub fn missed(&self) -> u64 {
        self.missed
    }

    /// Wake-ups left out as [`Seen::Early`].
    pub fn early(&self) -> u64 {
        self.early
    }
}

/// Whether `period_ns` is a display refresh's period.
fn plausible(period_ns: f64) -> bool {
    let min = VBLANK_MIN_PERIOD.as_nanos() as f64;
    let max = VBLANK_MAX_PERIOD.as_nanos() as f64;
    period_ns >= min && period_ns <= max
}

/// Least squares `t = a + b·k` over `samples` (at least two refreshes, so
/// the indices spread): the fitted time of the last sample's refresh, and
/// `b` (the period).
fn fit(samples: &VecDeque<(i64, f64)>) -> (f64, f64) {
    let last_k = samples.back().map_or(0, |&(k, _)| k);
    let n = samples.len() as f64;
    let mut k_sum = 0.0;
    let mut t_sum = 0.0;
    for &(k, t) in samples {
        k_sum += k as f64;
        t_sum += t;
    }
    let (k_mean, t_mean) = (k_sum / n, t_sum / n);
    // Σ dk·t, not Σ dk·(t − t̄): the dk sum to zero, so the two are equal.
    let mut kk = 0.0;
    let mut kt = 0.0;
    for &(k, t) in samples {
        let dk = k as f64 - k_mean;
        kk += dk * dk;
        kt += dk * t;
    }
    let period = kt / kk;
    (t_mean + period * (last_k as f64 - k_mean), period)
}

#[cfg(test)]
#[path = "vblank_tests.rs"]
mod tests;
