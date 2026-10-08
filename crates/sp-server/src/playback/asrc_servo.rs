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
//! the frames buffered for the card (the ring + the splice's hold) and the
//! frames the card consumed so far.
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
//!   slot, a changed delay, a driver buffer's sawtooth) is drained by the
//!   ratio, 33 ms in about 2.5 min within ±300 ppm. Inside the calm zone
//!   only camera-box's level loop acts.
//! - output: clamp(rate + slew + P + I, ±300 ppm), moved at most 5 ppm per
//!   second of the wall (camera-box #803: inaudible).
//! - jitter: a late or clumped block finds the ring that much emptier, so
//!   the latency it reads is the same — nothing moves. Only a lasting offset
//!   does, and a block's own reading never kicks the ratio: the level loop
//!   reads the 1 s window's mean.
//! - the last resort: a block more than [`HARD_DEFICIT_100NS`] short of the
//!   target (the ring would run dry on most blocks before the slew could
//!   restore it) or more than [`HARD_EXCESS_100NS`] over it (the next block
//!   would overflow the ring) is a HARD re-centre at once, by its error: a
//!   fault, counted (`hard_recentres`), WARNed by the worker, which inserts
//!   or skips under fades (`asrc::Splice`). The first block primes the ring
//!   to the target the same way, and that is no re-centre.
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
/// SongPlayer's last resort for a deficit: a block more than 50 ms (1.5
/// slots) short of the target is a hard re-centre. The ring then holds under
/// 11.7 ms for a block on time, so most blocks of the normal 10–33 ms
/// hand-off would find it dry before the slew could restore it; a date
/// step's remainder (under one slot) plus a driver's sawtooth stays under it.
pub const HARD_DEFICIT_100NS: i64 = 500_000;
/// SongPlayer's last resort for an excess: a block more than four slots
/// (133.3 ms) over the target is a hard re-centre — the ring holds the
/// target + 4 slots + one block, so the next block would overflow it.
pub const HARD_EXCESS_100NS: i64 = 1_333_333;
/// SongPlayer's: a window mean within this of the target is left to
/// camera-box's level loop; beyond it the offset slew drains it (at least;
/// [`calm_zone_ms`]).
pub const CALM_ZONE_MS: f64 = 1.0;

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
}

/// Why the worker inserts or skips before a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recentre {
    /// The first block: the ring primed to the target (no re-centre).
    Prime,
    /// A hard re-centre: more than [`HARD_DEFICIT_100NS`] short.
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
    /// The last window's latency less the target, ms: the offset the slew
    /// drains (positive = later than the target).
    pub offset_ms: f64,
    /// The seconds the slew still needs to bring the offset into the calm
    /// zone ([`slew_eta_s`]); `None` inside it.
    pub slew_eta_s: Option<f64>,
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
            return self.recentre(err_100ns, Recentre::Prime);
        };
        if let Some(hard) = hard_recentre(err_100ns) {
            return self.recentre(err_100ns, hard);
        }
        let x_100ns = o.handled_100ns - origin;
        let y_s = o.consumed_frames as f64 / self.rate_hz - x_100ns as f64 / 1e7;
        let Some(w) = self.window.add(x_100ns, y_s, latency_100ns) else {
            return self.hold();
        };
        if w.ppm.abs() > MAX_SANE_WINDOW_PPM {
            self.regression.flush();
            return self.hold();
        }
        if self.regression.offer(w.x_mean_s, w.y_mean_s) == Offered::Rebased {
            self.rebases += 1;
        }
        self.latency_ms = w.latency_mean_100ns as f64 / 10_000.0;
        let err_ms = (self.target_100ns - w.latency_mean_100ns) as f64 / 10_000.0;
        self.offset_ms = -err_ms;
        let dt_100ns = self
            .last_apply_100ns
            .map_or(w.span_100ns, |prev| w.x_end_100ns - prev);
        self.last_apply_100ns = Some(w.x_end_100ns);
        // A wall stepped back past the last window gives no time (the slew
        // already moves nothing; the EMA and the integral must not either).
        let dt_s = (dt_100ns as f64 / 1e7).max(0.0);
        let rate = self.regression.rate_ppm();
        // The offset slew beyond the calm zone; the level loop's I freezes
        // on the whole sum (its anti-windup counts the slew too).
        let drain = 0.0;
        let pi = self.level.update(err_ms, dt_s, rate + drain);
        let target = (rate + drain + pi).clamp(-MAX_PPM, MAX_PPM);
        self.applied_ppm = slew(self.applied_ppm, target, dt_s);
        self.slew_eta_s = slew_eta_s(
            err_ms,
            self.applied_ppm - rate,
            MAX_PPM - rate.abs(),
            self.calm_ms,
        );
        self.hold()
    }

    fn hold(&self) -> ServoAction {
        ServoAction {
            correction_ppm: self.applied_ppm,
            recentre_100ns: 0,
            recentre: None,
        }
    }

    /// Insert or skip `err_100ns` at once: the priming, or a hard re-centre
    /// (counted). The window and the level loop's error start over.
    fn recentre(&mut self, err_100ns: i64, why: Recentre) -> ServoAction {
        self.window = Window::default();
        self.level.reset_error();
        self.hard_recentres += 1;
        ServoAction {
            correction_ppm: self.applied_ppm,
            recentre_100ns: err_100ns,
            recentre: Some(why),
        }
    }
}

/// A block's error (target − latency) that the slew cannot be left with:
/// short by more than [`HARD_DEFICIT_100NS`], or over by more than
/// [`HARD_EXCESS_100NS`].
pub fn hard_recentre(err_100ns: i64) -> Option<Recentre> {
    if err_100ns > SLOT_100NS {
        Some(Recentre::Deficit)
    } else if err_100ns < -SLOT_100NS {
        Some(Recentre::Excess)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "asrc_servo_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "asrc_servo_sim_tests.rs"]
mod sim_tests;
