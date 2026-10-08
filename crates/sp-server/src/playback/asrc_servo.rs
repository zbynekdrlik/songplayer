//! #233: the ASIO output's drift servo — camera-box's `asrc-compensator`
//! (libobs `media-io/asrc-compensator.{h,c}`; its MIT Rust model
//! `camera-box/src/asrc_bench.rs` `RealtimeAsrcCompensator`; the line numbers
//! below are camera-box's) turned around for an OUTPUT. The producer is
//! SongPlayer's program (the genlock wall, dantesync); the consumer is the
//! card's clock (Dante PTP, SoundGrid). Pure: the ASIO worker feeds it one
//! observation per program block and gives its answer to the resampler
//! (`asrc.rs`).
//!
//! Per block, on the program wall: when the block was handled, its boundary,
//! the frames buffered for the card (the ring + the splice's hold), the
//! frames the card consumed so far and the frames it played silence for
//! (its underruns).
//!
//! - latency = (buffered − the splice's pending skip, signed) / rate +
//!   (handled − boundary): a boundary's time to its sound leaving SongPlayer.
//!   Target:
//!   two grid slots (VBAN's send budget) + the entry's delay — not "2–3
//!   driver buffers": one 33 ms block
//!   arrives per boundary, 10–33 ms late in normal operation, so a ring held
//!   at a few driver buffers would underrun on every block.
//! - rate point = (handled, consumed / rate − handled) per 1 s window (the
//!   window's means): the least-squares slope over up to 600 s is the card's
//!   ppm against the wall, used once 30 points span 60 s (camera-box #1084).
//! - level loop: P 2 ppm/ms on a 10 s EMA of the window-mean latency error,
//!   ±50 ppm (camera-box #1335 follow-up 5); I 0.0002 ppm/(ms·s), ±3 ppm,
//!   frozen while rate + P + I saturates (the plan's form: camera-box freezes
//!   on its estimate + bias + I, without P).
//! - the offset slew (the owner's ruling of 8.10.2026, #233 comment
//!   6053850076: the resampler absorbs a difference SMOOTHLY, by its ratio,
//!   never by a skip or an insert): a window mean beyond the calm zone
//!   ([`calm_zone_ms`]: 1 ms, or half the driver's callback period when that
//!   is longer — a 1 s mean of a reading that saws by a period wanders by a
//!   fraction of it) adds [`braking_ppm`] — the fastest
//!   correction that can still stop at the calm zone's edge decelerating at
//!   the slew limit — so a lasting offset (a date step's remainder ≤ one
//!   slot, a callback period's sawtooth) is drained by the ratio, 33 ms in
//!   about 2.5 min within ±300 ppm. Inside the calm zone only camera-box's
//!   level loop acts. (A changed delay rebuilds the output and a driver's
//!   buffer change reopens it: both start over with the priming.)
//! - the excess an underrun leaves is KEPT as cushion (the main session's
//!   ruling, #233 comment 6056680979, Q1): the card's underrun frames since
//!   the last block are folded into a cushion over the target, at most
//!   [`CUSHION_MAX_100NS`] (one slot); the offset slew drains only what lies
//!   outside the band [target, target + cushion] ([`slew_err_100ns`]), so
//!   the next late block finds the ring that much fuller; the level loop
//!   keeps the configured target, so the kept excess decays slowly through
//!   it (P ≤ 50 ppm), and the cushion follows the window mean down
//!   ([`kept_cushion`]). A window that folded an underrun keeps its cushion
//!   (its mean still holds the blocks before it).
//! - output: clamp(rate + slew + P + I, ±300 ppm), moved at most 5 ppm per
//!   second of the wall (camera-box #803: inaudible).
//! - jitter: a late or clumped block finds the ring that much emptier, so
//!   the latency it reads is the same — nothing moves. Only a lasting offset
//!   does, and a block's own reading never kicks the ratio: the level loop
//!   reads the 1 s window's mean.
//! - the last resort: a block whose latency is under [`HARD_FLOOR_100NS`]
//!   (the splice's hold + one slot, Q2: a missing boundary at delay 0 is one
//!   faded insert, not a run of underruns; an absolute floor, so an entry's
//!   delay never moves it) or more than
//!   [`HARD_EXCESS_100NS`] off the target either way (over it, the next
//!   block would overflow the ring; under it, a delayed output would play
//!   that early for many minutes) is a HARD re-centre at once, by its
//!   error: a
//!   fault, counted (`hard_recentres`), WARNed by the worker, which inserts
//!   or skips under fades (`asrc::Splice`). The first block primes the ring
//!   to the target the same way, and that is no re-centre. Either drops the
//!   cushion.
//! - a rate point more than 10 ms off the fit re-bases the regression
//!   (#1335 follow-up 2): it stays out, and the next point moves the line by
//!   the whole step (the same straddle splits a step across two window
//!   means); a whole step under 10 ms enters it as a point (camera-box's
//!   design).
//!
//! Integers (100 ns) where a boundary is pinned (latency, window span), f64
//! for the regression. Sign: a POSITIVE correction makes MORE output per
//! input — the card runs fast, or too little is buffered. rubato's relative
//! ratio is `1 + ppm·1e-6`.

use std::collections::VecDeque;

/// Bound on the applied correction, ppm (camera-box `asrc_bench.rs:204`, `asrc-compensator.h:42`).
pub const MAX_PPM: f64 = 300.0;
/// The applied correction moves at most this per second of wall (`:209`, `.h:48`).
pub const MAX_SLEW_PPM_PER_S: f64 = 5.0;
/// The rate regression's span, s (`:223`, `.h:62`).
pub const REGRESSION_SPAN_S: f64 = 600.0;
/// Points before a slope is used (`:228`, `.h:67`).
pub const REGRESSION_MIN_POINTS: usize = 30;
/// Span before a slope is used, s (`:235`, `.h:76`).
pub const REGRESSION_LOCK_SPAN_S: f64 = 60.0;
/// Points kept at most (`:243`, `.h:82`).
pub const REGRESSION_CAP: usize = 640;
/// The measurement window, 100 ns (`WINDOW_S` 1.0, `:470`, `.h:120`).
pub const WINDOW_100NS: i64 = 10_000_000;
/// A rate point this far off the fit is a step, s (`STEP_RESIDUAL_MS` 10, `:284`, `.h:149`).
pub const STEP_RESIDUAL_S: f64 = 0.010;
/// The level loop's P gain, ppm per ms (`:373`, `.h:232`).
pub const LEVEL_KP_PPM_PER_MS: f64 = 2.0;
/// The P term's clamp, ppm (`:380`, `.h:239`).
pub const LEVEL_KP_MAX_PPM: f64 = 50.0;
/// The EMA that smooths the level error before P, s (`:392`, `.h:251`).
pub const LEVEL_EMA_TAU_S: f64 = 10.0;
/// The level loop's I gain, ppm per (ms · s) (`:264`, `.h:130`).
pub const LEVEL_KI_PPM_PER_MS_S: f64 = 0.0002;
/// The I term's clamp, ppm (`:271`, `.h:137`).
pub const LEVEL_INTEGRAL_MAX_PPM: f64 = 3.0;
/// A window measuring more than this is starved, not a clock
/// (`MAX_SANE_INSTANTANEOUS_PPM`, `:447`, `.h:109`).
pub const MAX_SANE_WINDOW_PPM: f64 = 100_000.0;
/// SongPlayer's: one grid slot, one program block (a literal, pinned against
/// the genlock grid by a test).
pub const SLOT_100NS: i64 = 333_333;
/// SongPlayer's: the latency target before the entry's delay — two grid
/// slots, VBAN's send budget (`VBAN_SEND_LATENCY_100NS`, pinned by a test).
pub const BASE_LATENCY_100NS: i64 = 666_666;
/// SongPlayer's last resort for a deficit (the main session's ruling, #233
/// comment 6056680979, Q2): a block whose latency is under the splice's
/// 5 ms hold + one slot (38.3 ms) is a hard re-centre. A missing program
/// boundary at delay 0 (−33.3 ms, which with a sub-millisecond hand-off
/// only a real stall makes) reads under it: one faded insert, counted and
/// logged as a fault, instead of a run of unfaded underruns while the slew
/// restores the ring. ABSOLUTE (review round 2): an entry's delay raises the
/// target, never this floor — a delayed output 50 ms short still holds its
/// delay in the ring.
pub const HARD_FLOOR_100NS: i64 = 383_333;
/// SongPlayer's last resort for an offset: a block more than four slots
/// (133.3 ms) over the target is a hard re-centre — the ring holds the
/// target + 4 slots + one block, so the next block would overflow it. The
/// same edge under the target (review round 3): above a delay of 105 ms
/// it lies above the floor (at or under that delay the floor alone is the
/// edge), and slewing 4 slots takes ~8 min.
pub const HARD_EXCESS_100NS: i64 = 1_333_333;
/// The most an underrun's excess is kept as cushion over the target (Q1):
/// one slot. Beyond it the offset slew drains it.
pub const CUSHION_MAX_100NS: i64 = SLOT_100NS;
/// SongPlayer's: a window mean within this of the target is left to
/// camera-box's level loop; beyond it the offset slew drains it (at least;
/// [`calm_zone_ms`]).
pub const CALM_ZONE_MS: f64 = 1.0;

/// The cushion after `fresh_100ns` more of the card's underruns: the excess
/// each one leaves added, at most [`CUSHION_MAX_100NS`].
pub fn fold_cushion(cushion_100ns: i64, fresh_100ns: i64) -> i64 {
    cushion_100ns
        .saturating_add(fresh_100ns)
        .min(CUSHION_MAX_100NS)
}

/// The cushion after a window whose mean latency was `latency_100ns`: never
/// more than what that window still held over `target_100ns`, so it follows
/// the level loop's drain down (and is gone once the latency is at or under
/// the target).
pub fn kept_cushion(cushion_100ns: i64, latency_100ns: i64, target_100ns: i64) -> i64 {
    cushion_100ns.min((latency_100ns - target_100ns).max(0))
}

/// The error the offset slew drains (target − latency sense: positive = too
/// little buffered): the latency's distance from the band [target, target +
/// cushion], 0 inside it. A kept cushion is not drained by the stop curve;
/// a deficit is measured from the target itself.
pub fn slew_err_100ns(latency_100ns: i64, target_100ns: i64, cushion_100ns: i64) -> i64 {
    let top = target_100ns + cushion_100ns;
    (target_100ns - latency_100ns).max(0) - (latency_100ns - top).max(0)
}

/// The calm zone for a driver calling back every `callback_frames` at
/// `rate_hz`: [`CALM_ZONE_MS`], or half the callback period when that is
/// longer (512 frames at 96 kHz: 2.67 ms) — the block's reading saws by one
/// period, and its window mean wanders by a fraction of it.
pub fn calm_zone_ms(callback_frames: u32, rate_hz: f64) -> f64 {
    (f64::from(callback_frames) / rate_hz * 1000.0 / 2.0).max(CALM_ZONE_MS)
}

/// What [`RateRegression::offer`] did with a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offered {
    Inserted,
    /// A step once the fit has its points: the point stays out, and the next
    /// one moves the line onto the step (the slope kept).
    Rebased,
    /// The point after a re-base: the line moves by its whole residual — a
    /// step inside a 1 s window shows partly in that window's mean and fully
    /// in the next one's.
    Realigned,
    /// A step before the fit has its points: the points start over.
    Restarted,
}

/// The card's rate: ordinary least squares over up to [`REGRESSION_SPAN_S`].
#[derive(Debug, Default)]
pub struct RateRegression {
    points: VecDeque<(f64, f64)>,
    offset_s: f64,
    /// The last point was a step: the next one realigns.
    realign_next: bool,
}

impl RateRegression {
    pub fn point_count(&self) -> usize {
        self.points.len()
    }

    fn span_s(&self) -> f64 {
        match (self.points.front(), self.points.back()) {
            (Some(a), Some(b)) => b.0 - a.0,
            _ => 0.0,
        }
    }

    /// (slope, intercept, x origin), the slope per second of x. The x values
    /// are centred once, so the covariance needs no second centring.
    fn fit(&self) -> Option<(f64, f64, f64)> {
        let x0 = self.points.front()?.0;
        let n = self.points.len() as f64;
        let mx = self.points.iter().map(|p| p.0 - x0).sum::<f64>() / n;
        let my = self.points.iter().map(|p| p.1).sum::<f64>() / n;
        let (sxx, sxy) = self.points.iter().fold((0.0, 0.0), |(sxx, sxy), p| {
            let dx = p.0 - x0 - mx;
            (sxx + dx * dx, sxy + dx * p.1)
        });
        if sxx == 0.0 {
            return None;
        }
        let slope = sxy / sxx;
        Some((slope, my - slope * mx, x0))
    }

    pub fn locked(&self) -> bool {
        self.points.len() >= REGRESSION_MIN_POINTS && self.span_s() >= REGRESSION_LOCK_SPAN_S
    }

    /// The card's ppm against the wall once locked, else 0.
    pub fn rate_ppm(&self) -> f64 {
        if !self.locked() {
            return 0.0;
        }
        self.fit().map_or(0.0, |(slope, _, _)| slope * 1e6)
    }

    pub fn offer(&mut self, x_s: f64, y_raw_s: f64) -> Offered {
        let y = y_raw_s - self.offset_s;
        if self.points.len() >= REGRESSION_MIN_POINTS
            && let Some((slope, intercept, x0)) = self.fit()
        {
            let residual = y - (intercept + slope * (x_s - x0));
            // The point after a step moves the line by the whole step,
            // whatever is left of it: one step re-bases once.
            if self.realign_next {
                self.offset_s += residual;
                self.realign_next = false;
                return Offered::Realigned;
            }
            if residual.abs() > STEP_RESIDUAL_S {
                self.realign_next = true;
                return Offered::Rebased;
            }
        } else if let Some(&(_, last)) = self.points.back()
            && (y - last).abs() > STEP_RESIDUAL_S
        {
            self.flush();
            self.points.push_back((x_s, y_raw_s));
            return Offered::Restarted;
        }
        self.points.push_back((x_s, y));
        let over_cap = self.points.len().saturating_sub(REGRESSION_CAP);
        self.points.drain(..over_cap);
        // Every pass pops a point, so it ends (a mutant can only empty it).
        while let Some(&(oldest, _)) = self.points.front()
            && x_s - oldest > REGRESSION_SPAN_S
        {
            self.points.pop_front();
        }
        Offered::Inserted
    }

    pub fn flush(&mut self) {
        self.points.clear();
        self.offset_s = 0.0;
        self.realign_next = false;
    }
}

/// The level loop's P + I (camera-box #1335 follow-up 5).
#[derive(Debug, Default)]
pub struct LevelLoop {
    ema_ms: f64,
    integral_ppm: f64,
}

impl LevelLoop {
    /// P + I after one window of `dt_s` whose latency error is `err_ms`
    /// (target − latency: positive = too little buffered = more output).
    pub fn update(&mut self, err_ms: f64, dt_s: f64, rate_ppm: f64) -> f64 {
        let alpha = dt_s / (LEVEL_EMA_TAU_S + dt_s);
        self.ema_ms += alpha * (err_ms - self.ema_ms);
        let p = (LEVEL_KP_PPM_PER_MS * self.ema_ms).clamp(-LEVEL_KP_MAX_PPM, LEVEL_KP_MAX_PPM);
        if (rate_ppm + p + self.integral_ppm).abs() < MAX_PPM {
            self.integral_ppm = (self.integral_ppm + LEVEL_KI_PPM_PER_MS_S * err_ms * dt_s)
                .clamp(-LEVEL_INTEGRAL_MAX_PPM, LEVEL_INTEGRAL_MAX_PPM);
        }
        p + self.integral_ppm
    }

    /// After a re-centre: the error the EMA held is gone.
    pub fn reset_error(&mut self) {
        self.ema_ms = 0.0;
    }

    pub fn ema_ms(&self) -> f64 {
        self.ema_ms
    }

    pub fn integral_ppm(&self) -> f64 {
        self.integral_ppm
    }
}

/// `applied` moved toward `target` by at most [`MAX_SLEW_PPM_PER_S`] × `dt_s`
/// (a backward wall moves nothing).
pub fn slew(applied: f64, target: f64, dt_s: f64) -> f64 {
    let step = MAX_SLEW_PPM_PER_S * dt_s.max(0.0);
    applied + (target - applied).clamp(-step, step)
}

/// The offset slew for a window-mean error `err_ms` (target − latency:
/// positive = too little buffered = more output): beyond the calm zone
/// (`calm_ms`), the fastest correction that still stops at its edge
/// decelerating at the slew limit, `√(2 · 5 ppm/s · (|err| − calm))` (1 ms =
/// 1000 ppm·s); 0 inside it.
pub fn braking_ppm(err_ms: f64, calm_ms: f64) -> f64 {
    let beyond_ms = (err_ms.abs() - calm_ms).max(0.0);
    (2.0 * MAX_SLEW_PPM_PER_S * beyond_ms * 1000.0)
        .sqrt()
        .copysign(err_ms)
}

/// The seconds the offset slew still needs to bring a window-mean error
/// `err_ms` into the calm zone (`calm_ms`): `share_ppm` of the correction already works
/// on it (the applied correction less the card's rate; against it when its
/// sign differs), at most `room_ppm` can (the ±300 budget less the rate),
/// moving at most 5 ppm per second — accelerate to a peak (at most the
/// room), cruise, brake. `None` inside the calm zone (nothing to slew).
pub fn slew_eta_s(err_ms: f64, share_ppm: f64, room_ppm: f64, calm_ms: f64) -> Option<f64> {
    let beyond_ms = err_ms.abs() - calm_ms;
    if beyond_ms <= 0.0 {
        return None;
    }
    let a = MAX_SLEW_PPM_PER_S;
    let cap = room_ppm.max(a);
    let toward = if err_ms.is_sign_positive() {
        share_ppm
    } else {
        -share_ppm
    };
    // Working the wrong way: stop first, and win back what that adds.
    let back = (-toward).max(0.0);
    let distance = beyond_ms * 1000.0 + back * back / (2.0 * a);
    let speed = toward.clamp(0.0, cap);
    // A speed whose stop already covers the distance only brakes (the peak
    // is the speed itself).
    let peak = (a * distance + speed * speed / 2.0)
        .sqrt()
        .clamp(speed, cap);
    let cruise = ((distance - (2.0 * peak * peak - speed * speed) / (2.0 * a)) / peak).max(0.0);
    Some(back / a + (peak - speed) / a + cruise + peak / a)
}

/// One observation, taken by the worker as it handles a program block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    /// When the block was handled, on the program wall (100 ns).
    pub handled_100ns: i64,
    /// The block's boundary (its stamp).
    pub stamp_100ns: i64,
    /// Frames buffered for the card: the ring + the splice's hold.
    pub buffered_frames: u64,
    /// Frames the splice will still drop from the coming blocks' output
    /// (`asrc::Splice::pending_skip_frames`): counted against the buffered
    /// frames — signed, since after a stall they can outnumber them. A skip
    /// longer than one block runs over several and is not asked for again.
    pub pending_skip_frames: u64,
    /// Frames the card consumed since the output opened.
    pub consumed_frames: u64,
    /// Frames the card played silence for, since the output opened (its
    /// underruns; the worker counts a short callback as a whole buffer, an
    /// overcount of at most one buffer per event, accepted by the ruling).
    pub underrun_frames: u64,
}

/// Why the worker inserts or skips before a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recentre {
    /// The first block: the ring primed to the target (no re-centre).
    Prime,
    /// A hard re-centre: the latency under [`HARD_FLOOR_100NS`], or more
    /// than [`HARD_EXCESS_100NS`] under the target.
    Deficit,
    /// A hard re-centre: more than [`HARD_EXCESS_100NS`] over.
    Excess,
}

impl Recentre {
    /// The status's and the log's word.
    pub fn as_str(self) -> &'static str {
        match self {
            Recentre::Prime => "prime",
            Recentre::Deficit => "deficit",
            Recentre::Excess => "excess",
        }
    }
}

/// What the worker does with the block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ServoAction {
    /// The resampler's correction, ppm (relative ratio `1 + ppm·1e-6`).
    pub correction_ppm: f64,
    /// Insert (> 0) or skip (< 0) this much before the block, 100 ns.
    pub recentre_100ns: i64,
    /// Why (`None`: nothing is inserted or skipped).
    pub recentre: Option<Recentre>,
}

/// The servo's state for the status (`outputs[i].asio`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ServoStatus {
    pub correction_ppm: f64,
    /// The card's rate against the wall; 0 until the regression locks.
    pub rate_ppm: f64,
    pub locked: bool,
    /// The last window's mean latency (boundary → leaving SongPlayer), ms.
    pub latency_ms: f64,
    /// The offset the slew drains, ms: the last window's latency outside the
    /// band [target, target + cushion] (positive = later than the band, 0
    /// inside it).
    pub offset_ms: f64,
    /// The seconds the slew still needs to bring the offset into the calm
    /// zone ([`slew_eta_s`]); `None` inside it.
    pub slew_eta_s: Option<f64>,
    /// The excess the card's underruns left, kept over the target (Q1), ms:
    /// not an offset the slew drains, it decays through the level loop.
    pub cushion_ms: f64,
    /// Hard re-centres (faults; the first block's priming is none).
    pub hard_recentres: u64,
    pub rebases: u64,
}

/// `frames` (negative: owed) at `rate_hz` in 100 ns, rounded.
pub fn frames_to_100ns(frames: i64, rate_hz: f64) -> i64 {
    (frames as f64 * 1e7 / rate_hz).round() as i64
}

/// `v_100ns` at `rate_hz` in frames, rounded: a re-centre's frames (positive
/// = insert, negative = skip).
pub fn frames_from_100ns(v_100ns: i64, rate_hz: f64) -> i64 {
    (v_100ns as f64 * rate_hz / 1e7).round() as i64
}

/// One window's sums; it closes once it spans [`WINDOW_100NS`].
#[derive(Debug, Default)]
struct Window {
    n: i64,
    first: Option<(i64, f64)>,
    sum_x_s: f64,
    sum_y_s: f64,
    sum_latency_100ns: i64,
}

/// A closed window.
struct Closed {
    x_mean_s: f64,
    y_mean_s: f64,
    x_end_100ns: i64,
    span_100ns: i64,
    latency_mean_100ns: i64,
    /// The card against the wall over this window alone (the starvation check).
    ppm: f64,
}

impl Window {
    /// Add one observation (`x` relative to the servo's origin); a window
    /// spanning [`WINDOW_100NS`] closes and starts over.
    fn add(&mut self, x_100ns: i64, y_s: f64, latency_100ns: i64) -> Option<Closed> {
        if self.first.is_some_and(|(x0, _)| x_100ns < x0) {
            // The wall went back before this window began: start it over.
            *self = Self::default();
        }
        let (x0, y0) = *self.first.get_or_insert((x_100ns, y_s));
        self.n += 1;
        self.sum_x_s += x_100ns as f64 / 1e7;
        self.sum_y_s += y_s;
        self.sum_latency_100ns += latency_100ns;
        let span_100ns = x_100ns - x0;
        if span_100ns < WINDOW_100NS {
            return None;
        }
        let n = self.n as f64;
        let closed = Closed {
            x_mean_s: self.sum_x_s / n,
            y_mean_s: self.sum_y_s / n,
            x_end_100ns: x_100ns,
            span_100ns,
            latency_mean_100ns: self.sum_latency_100ns / self.n,
            ppm: (y_s - y0) / (span_100ns as f64 / 1e7) * 1e6,
        };
        *self = Self::default();
        Some(closed)
    }
}

/// The servo of one ASIO output (see the module doc).
pub struct Servo {
    rate_hz: f64,
    target_100ns: i64,
    origin_100ns: Option<i64>,
    window: Window,
    regression: RateRegression,
    level: LevelLoop,
    /// [`calm_zone_ms`] of the driver (1 ms until told its callback size).
    calm_ms: f64,
    applied_ppm: f64,
    last_apply_100ns: Option<i64>,
    latency_ms: f64,
    offset_ms: f64,
    slew_eta_s: Option<f64>,
    hard_recentres: u64,
    rebases: u64,
    /// The excess the card's underruns left, kept over the target (Q1).
    cushion_100ns: i64,
    /// The card's underrun frames already folded (from the observation).
    underruns_seen: u64,
    /// The current window folded an underrun: its mean still holds the
    /// blocks before it, so it does not cut the cushion.
    folded: bool,
}

impl Servo {
    /// For a card at `device_rate_hz`, holding `target_latency_100ns`
    /// ([`BASE_LATENCY_100NS`] + the entry's delay).
    pub fn new(device_rate_hz: f64, target_latency_100ns: i64) -> Self {
        Self {
            rate_hz: device_rate_hz,
            target_100ns: target_latency_100ns,
            origin_100ns: None,
            window: Window::default(),
            regression: RateRegression::default(),
            level: LevelLoop::default(),
            calm_ms: CALM_ZONE_MS,
            applied_ppm: 0.0,
            last_apply_100ns: None,
            latency_ms: 0.0,
            offset_ms: 0.0,
            slew_eta_s: None,
            hard_recentres: 0,
            rebases: 0,
            cushion_100ns: 0,
            underruns_seen: 0,
            folded: false,
        }
    }

    /// The calm zone of a driver calling back every `callback_frames`
    /// ([`calm_zone_ms`]).
    pub fn with_callback_frames(mut self, callback_frames: u32) -> Self {
        self.calm_ms = calm_zone_ms(callback_frames, self.rate_hz);
        self
    }

    pub fn target_100ns(&self) -> i64 {
        self.target_100ns
    }

    pub fn calm_ms(&self) -> f64 {
        self.calm_ms
    }

    /// The rate and the lock are read from the regression itself, so a flush
    /// (a starved window) shows at once.
    pub fn status(&self) -> ServoStatus {
        ServoStatus {
            correction_ppm: self.applied_ppm,
            rate_ppm: self.regression.rate_ppm(),
            locked: self.regression.locked(),
            latency_ms: self.latency_ms,
            offset_ms: self.offset_ms,
            slew_eta_s: self.slew_eta_s,
            cushion_ms: self.cushion_100ns as f64 / 10_000.0,
            hard_recentres: self.hard_recentres,
            rebases: self.rebases,
        }
    }

    pub fn observe(&mut self, o: Observation) -> ServoAction {
        let to_play = o.buffered_frames as i64 - o.pending_skip_frames as i64;
        let latency_100ns =
            frames_to_100ns(to_play, self.rate_hz) + (o.handled_100ns - o.stamp_100ns);
        let err_100ns = self.target_100ns - latency_100ns;
        let Some(origin) = self.origin_100ns else {
            self.origin_100ns = Some(o.handled_100ns);
            self.underruns_seen = o.underrun_frames;
            return self.recentre(err_100ns, Recentre::Prime);
        };
        self.fold_underruns(o.underrun_frames);
        if let Some(hard) = hard_recentre(latency_100ns, self.target_100ns) {
            return self.recentre(err_100ns, hard);
        }
        let x_100ns = o.handled_100ns - origin;
        let y_s = o.consumed_frames as f64 / self.rate_hz - x_100ns as f64 / 1e7;
        let Some(w) = self.window.add(x_100ns, y_s, latency_100ns) else {
            return self.hold();
        };
        let folded = std::mem::take(&mut self.folded);
        if w.ppm.abs() > MAX_SANE_WINDOW_PPM {
            self.regression.flush();
            return self.hold();
        }
        if self.regression.offer(w.x_mean_s, w.y_mean_s) == Offered::Rebased {
            self.rebases += 1;
        }
        if !folded {
            self.cushion_100ns =
                kept_cushion(self.cushion_100ns, w.latency_mean_100ns, self.target_100ns);
        }
        self.latency_ms = w.latency_mean_100ns as f64 / 10_000.0;
        // The level loop keeps the configured target (a kept cushion decays
        // through it); the offset slew drains only what lies outside the
        // cushion's band.
        let err_ms = (self.target_100ns - w.latency_mean_100ns) as f64 / 10_000.0;
        let slew_ms = slew_err_100ns(w.latency_mean_100ns, self.target_100ns, self.cushion_100ns)
            as f64
            / 10_000.0;
        self.offset_ms = -slew_ms;
        let dt_100ns = self
            .last_apply_100ns
            .map_or(w.span_100ns, |prev| w.x_end_100ns - prev);
        self.last_apply_100ns = Some(w.x_end_100ns);
        // A wall stepped back past the last window gives no time (the slew
        // already moves nothing; the EMA and the integral must not either).
        let dt_s = (dt_100ns as f64 / 1e7).max(0.0);
        let rate = self.regression.rate_ppm();
        // The card's rate plus the offset slew beyond the calm zone; the
        // level loop's I freezes on the whole sum (its anti-windup counts the
        // slew too).
        let base = rate + braking_ppm(slew_ms, self.calm_ms);
        let pi = self.level.update(err_ms, dt_s, base);
        let target = (base + pi).clamp(-MAX_PPM, MAX_PPM);
        self.applied_ppm = slew(self.applied_ppm, target, dt_s);
        // The room the card's rate leaves on the side the slew works: a
        // positive error asks for more output, up to +300 (review round 2).
        let room = if slew_ms.is_sign_positive() {
            MAX_PPM - rate
        } else {
            MAX_PPM + rate
        };
        self.slew_eta_s = slew_eta_s(slew_ms, self.applied_ppm - rate, room, self.calm_ms);
        self.hold()
    }

    /// The card's underrun frames since the last block (`underrun_frames`
    /// is cumulative) are folded into the cushion ([`fold_cushion`]); the
    /// current window then keeps it.
    fn fold_underruns(&mut self, underrun_frames: u64) {
        let fresh = underrun_frames.saturating_sub(self.underruns_seen);
        self.underruns_seen = self.underruns_seen.max(underrun_frames);
        if fresh > 0 {
            let fresh_100ns =
                frames_to_100ns(i64::try_from(fresh).unwrap_or(i64::MAX), self.rate_hz);
            self.cushion_100ns = fold_cushion(self.cushion_100ns, fresh_100ns);
            self.folded = true;
        }
    }

    fn hold(&self) -> ServoAction {
        ServoAction {
            correction_ppm: self.applied_ppm,
            recentre_100ns: 0,
            recentre: None,
        }
    }

    /// Insert or skip `err_100ns` at once: the priming, or a hard re-centre
    /// (counted). The window, the level loop's error and the cushion start
    /// over (the latency is at the target again).
    fn recentre(&mut self, err_100ns: i64, why: Recentre) -> ServoAction {
        self.window = Window::default();
        self.level.reset_error();
        self.cushion_100ns = 0;
        self.folded = false;
        if why != Recentre::Prime {
            self.hard_recentres += 1;
        }
        ServoAction {
            correction_ppm: self.applied_ppm,
            recentre_100ns: err_100ns,
            recentre: Some(why),
        }
    }
}

/// A block's latency the slew cannot be left with: under
/// [`HARD_FLOOR_100NS`], or more than [`HARD_EXCESS_100NS`] off `target`
/// either way.
pub fn hard_recentre(latency_100ns: i64, target_100ns: i64) -> Option<Recentre> {
    let over = latency_100ns - target_100ns;
    if latency_100ns < HARD_FLOOR_100NS || -over > HARD_EXCESS_100NS {
        Some(Recentre::Deficit)
    } else if over > HARD_EXCESS_100NS {
        Some(Recentre::Excess)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "asrc_servo_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "asrc_servo_tests_regression.rs"]
mod tests_regression;

#[cfg(test)]
#[path = "asrc_servo_sim_tests.rs"]
mod sim_tests;

#[cfg(test)]
#[path = "asrc_servo_tests_cushion.rs"]
mod tests_cushion;
