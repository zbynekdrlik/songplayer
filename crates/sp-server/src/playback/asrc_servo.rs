//! #233: the ASIO output's drift servo. Since #285 step 3 an ADAPTER over
//! dstack-resample's ASRC controller (`dstack_resample::asrc::Controller`):
//! the laws — camera-box's `asrc-compensator` turned around for an output,
//! with SongPlayer's cushion, calm zone and priming — live there, made
//! generic (two clocks and a target depth); this file maps SongPlayer's
//! observation (100 ns on the program wall, frames at the card's rate) onto
//! it and keeps the worker's and the status's API. The producer is
//! SongPlayer's program (the genlock wall, dantesync); the consumer is the
//! card's clock (Dante PTP, SoundGrid). Pure: the ASIO worker feeds it one
//! observation per program block and gives its answer to the resampler
//! (`asrc.rs`).
//!
//! Per block, on the program wall: when the block was handled, its boundary,
//! the frames buffered for the card (the ring + the splice's hold), the
//! frames the card consumed so far and the frames it played silence for
//! (its underruns). The adapter gives the controller:
//!
//! - the depth = latency = (buffered − the splice's pending skip, signed) /
//!   rate + (handled − boundary): a boundary's time to its sound leaving
//!   SongPlayer. Target: two grid slots (VBAN's send budget) + the entry's
//!   delay — not "2–3 driver buffers": one 33 ms block arrives per boundary,
//!   10–33 ms late in normal operation;
//! - "our" clock = handled − the first block's handled, s; the other clock =
//!   consumed / rate (the rate point per 1 s window, the card's ppm);
//! - the underrun frames / rate as the consumer's silence (the cushion, at
//!   most [`CUSHION_MAX_100NS`], Q1);
//! - the calm zone [`calm_zone_ms`] of the driver's callback, the floor
//!   [`HARD_FLOOR_100NS`] and the edge [`HARD_EXCESS_100NS`], and the
//!   priming of the first block.
//!
//! dstack's laws (its `asrc.rs` module doc): the rate by least squares over
//! ≤ 600 s of 1 s window means (30 points over 60 s to lock, a 10 ms
//! residual re-bases); P 2 ppm/ms on a 10 s EMA (±50), I ±3 frozen while the
//! sum saturates; beyond the calm zone [`braking_ppm`] (the owner's ruling
//! of 8.10.2026: an offset is drained by the ratio, never by a skip or an
//! insert) against the band [target, target + cushion]; ±300 ppm, 5 ppm/s;
//! a window whose rate point moves > 100 000 ppm from the last is a stall
//! (the fit flushed, the correction held); the last resort, a depth under
//! the floor or more than the edge off the target, is a HARD re-centre at
//! once by its error (a fault, counted, WARNed by the worker, which inserts
//! or skips under fades, `asrc::Splice`). The first block primes the ring to
//! the target the same way, and that is no re-centre.
//!
//! What stays SongPlayer's here: the 100 ns observation and its latency
//! reading, the status (the window's mean latency, [`slew_eta_s`]), and the
//! constants of its grid. Sign: a POSITIVE correction makes MORE output per
//! input — the card runs fast, or too little is buffered. The resampler's
//! relative ratio is `1 + ppm·1e-6`.

use dstack_resample::asrc::{Basis, Controller, RateFit, Step, WINDOW_S, calm_zone_ms_for};

pub use dstack_resample::asrc::{
    CALM_ZONE_MS, LEVEL_EMA_TAU_S, LEVEL_INTEGRAL_MAX_PPM, LEVEL_KI_PPM_PER_MS_S, LEVEL_KP_MAX_PPM,
    LEVEL_KP_PPM_PER_MS, LevelLoop, MAX_PPM, MAX_SANE_WINDOW_PPM, MAX_SLEW_PPM_PER_S, Offered,
    REGRESSION_CAP, REGRESSION_LOCK_SPAN_S, REGRESSION_MIN_POINTS, REGRESSION_SPAN_S,
    STEP_RESIDUAL_S, braking_ppm, slew,
};

/// The measurement window, 100 ns: dstack's `WINDOW_S` (1 s).
pub const WINDOW_100NS: i64 = 10_000_000;
const _: () = assert!(WINDOW_S == 1.0);
/// SongPlayer's: one grid slot, one program block (a literal, pinned against
/// the genlock grid by a test).
pub const SLOT_100NS: i64 = 333_333;
/// SongPlayer's: the latency target before the entry's delay — two grid
/// slots, VBAN's send budget (`VBAN_SEND_LATENCY_100NS`, pinned by a test).
pub const BASE_LATENCY_100NS: i64 = 666_666;
/// SongPlayer's last resort for a deficit (the main session's ruling, #233
/// comment 6056680979, Q2): a block whose latency is under the splice's
/// 5 ms hold + one slot (38.3 ms) is a hard re-centre. A missing program
/// boundary at delay 0 reads under it: one faded insert, counted and logged
/// as a fault, instead of a run of unfaded underruns. ABSOLUTE (review round
/// 2): an entry's delay raises the target, never this floor.
pub const HARD_FLOOR_100NS: i64 = 383_333;
/// SongPlayer's last resort for an offset: a block more than four slots
/// (133.3 ms) off the target either way is a hard re-centre — the ring
/// holds the target + 4 slots + one block, so the next block would overflow
/// it; under the target a delayed output would play that early for minutes.
pub const HARD_EXCESS_100NS: i64 = 1_333_333;
/// The most an underrun's excess is kept as cushion over the target (Q1):
/// one slot. Beyond it the offset slew drains it.
pub const CUSHION_MAX_100NS: i64 = SLOT_100NS;

/// The cushion after `fresh_100ns` more of the card's underruns: the excess
/// each one leaves added, at most [`CUSHION_MAX_100NS`] (dstack's
/// `played_silence`, in 100 ns).
pub fn fold_cushion(cushion_100ns: i64, fresh_100ns: i64) -> i64 {
    cushion_100ns
        .saturating_add(fresh_100ns)
        .min(CUSHION_MAX_100NS)
}

/// The cushion after a window whose mean latency was `latency_100ns`: never
/// more than what that window still held over `target_100ns` (dstack's
/// window close, in 100 ns).
pub fn kept_cushion(cushion_100ns: i64, latency_100ns: i64, target_100ns: i64) -> i64 {
    cushion_100ns.min((latency_100ns - target_100ns).max(0))
}

/// The error the offset slew drains (target − latency sense: positive = too
/// little buffered): the latency's distance from the band [target, target +
/// cushion], 0 inside it (dstack's `band_error_ms`, in 100 ns).
pub fn slew_err_100ns(latency_100ns: i64, target_100ns: i64, cushion_100ns: i64) -> i64 {
    let top = target_100ns + cushion_100ns;
    (target_100ns - latency_100ns).max(0) - (latency_100ns - top).max(0)
}

/// The calm zone for a driver calling back every `callback_frames` at
/// `rate_hz`: dstack's `calm_zone_ms_for` — [`CALM_ZONE_MS`], or half the
/// callback period when that is longer (512 frames at 96 kHz: 2.67 ms).
pub fn calm_zone_ms(callback_frames: u32, rate_hz: f64) -> f64 {
    calm_zone_ms_for(f64::from(callback_frames) / rate_hz)
}

/// The card's rate: dstack's least squares over up to [`REGRESSION_SPAN_S`].
#[derive(Debug, Default)]
pub struct RateRegression(RateFit);

impl RateRegression {
    pub fn point_count(&self) -> usize {
        self.0.points()
    }

    pub fn locked(&self) -> bool {
        self.0.locked()
    }

    /// The card's ppm against the wall once locked, else 0.
    pub fn rate_ppm(&self) -> f64 {
        self.0.rate_ppm()
    }

    pub fn offer(&mut self, x_s: f64, y_raw_s: f64) -> Offered {
        self.0.offer(x_s, y_raw_s)
    }

    pub fn flush(&mut self) {
        self.0.flush();
    }
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

/// `ms` on the observation's 100 ns grain (the status shows what the wall
/// can tell apart; the controller computes in `f64` seconds).
fn to_100ns_grain(ms: f64) -> f64 {
    (ms * 10_000.0).round() / 10_000.0
}

/// The adapter's own copy of the controller's open window, for the status
/// only (the window's mean latency, the time left): it opens, closes and
/// starts over exactly where dstack's does.
#[derive(Debug, Default)]
struct Shadow {
    start_s: Option<f64>,
    sum_latency_100ns: i64,
    n: i64,
}

impl Shadow {
    /// One observation at `now_s`; the window's mean latency once it closes.
    fn add(&mut self, now_s: f64, latency_100ns: i64) -> Option<i64> {
        if self.start_s.is_some_and(|start| now_s < start) {
            *self = Self::default();
        }
        let start = *self.start_s.get_or_insert(now_s);
        self.sum_latency_100ns += latency_100ns;
        self.n += 1;
        if now_s - start < WINDOW_S {
            return None;
        }
        let mean = self.sum_latency_100ns / self.n;
        *self = Self::default();
        Some(mean)
    }
}

/// The servo of one ASIO output (see the module doc).
pub struct Servo {
    rate_hz: f64,
    target_100ns: i64,
    origin_100ns: Option<i64>,
    controller: Controller,
    /// [`calm_zone_ms`] of the driver (1 ms until told its callback size).
    calm_ms: f64,
    shadow: Shadow,
    latency_ms: f64,
    slew_eta_s: Option<f64>,
    /// The controller's stalls already seen: a window that is one shows no
    /// new figures.
    stalls_seen: u64,
}

impl Servo {
    /// For a card at `device_rate_hz`, holding `target_latency_100ns`
    /// ([`BASE_LATENCY_100NS`] + the entry's delay).
    pub fn new(device_rate_hz: f64, target_latency_100ns: i64) -> Self {
        let controller = Controller::new(
            target_latency_100ns as f64 / 1e7,
            HARD_FLOOR_100NS as f64 / 1e7,
            HARD_EXCESS_100NS as f64 / 1e7,
            Basis::Arrival,
        )
        .with_cushion_max_s(CUSHION_MAX_100NS as f64 / 1e7)
        .with_calm_zone_ms(CALM_ZONE_MS)
        .with_priming();
        Self {
            rate_hz: device_rate_hz,
            target_100ns: target_latency_100ns,
            origin_100ns: None,
            controller,
            calm_ms: CALM_ZONE_MS,
            shadow: Shadow::default(),
            latency_ms: 0.0,
            slew_eta_s: None,
            stalls_seen: 0,
        }
    }

    /// The calm zone of a driver calling back every `callback_frames`
    /// ([`calm_zone_ms`]).
    pub fn with_callback_frames(mut self, callback_frames: u32) -> Self {
        self.calm_ms = calm_zone_ms(callback_frames, self.rate_hz);
        self.controller = self.controller.with_calm_zone_ms(self.calm_ms);
        self
    }

    pub fn target_100ns(&self) -> i64 {
        self.target_100ns
    }

    pub fn calm_ms(&self) -> f64 {
        self.calm_ms
    }

    /// The rate and the lock are read from the controller itself, so a flush
    /// (a starved window) shows at once.
    pub fn status(&self) -> ServoStatus {
        let c = &self.controller;
        ServoStatus {
            correction_ppm: c.ppm(),
            rate_ppm: c.rate_ppm(),
            locked: c.locked(),
            latency_ms: self.latency_ms,
            offset_ms: to_100ns_grain(c.offset_ms()),
            slew_eta_s: self.slew_eta_s,
            cushion_ms: to_100ns_grain(c.cushion_s() * 1000.0),
            hard_recentres: c.recentres(),
            rebases: c.rebases(),
        }
    }

    pub fn observe(&mut self, o: Observation) -> ServoAction {
        let to_play = o.buffered_frames as i64 - o.pending_skip_frames as i64;
        let latency_100ns =
            frames_to_100ns(to_play, self.rate_hz) + (o.handled_100ns - o.stamp_100ns);
        let err_100ns = self.target_100ns - latency_100ns;
        let origin = *self.origin_100ns.get_or_insert(o.handled_100ns);
        let now_s = (o.handled_100ns - origin) as f64 / 1e7;
        self.controller
            .played_silence(o.underrun_frames as f64 / self.rate_hz);
        let step = self.controller.observe(
            now_s,
            latency_100ns as f64 / 1e7,
            o.consumed_frames as f64 / self.rate_hz,
        );
        let (correction_ppm, recentre) = match step {
            Step::Hold(ppm) => {
                self.window_status(now_s, latency_100ns);
                (ppm, None)
            }
            Step::Prime { ppm, .. } => (ppm, Some(Recentre::Prime)),
            Step::Recentre { by_s, ppm } if by_s > 0.0 => (ppm, Some(Recentre::Deficit)),
            Step::Recentre { ppm, .. } => (ppm, Some(Recentre::Excess)),
        };
        if recentre.is_some() {
            self.shadow = Shadow::default();
        }
        ServoAction {
            correction_ppm,
            recentre_100ns: if recentre.is_some() { err_100ns } else { 0 },
            recentre,
        }
    }

    /// The status's figures of a window the controller closed: its mean
    /// latency and the slew's time left (none for a stall).
    fn window_status(&mut self, now_s: f64, latency_100ns: i64) {
        let Some(mean_100ns) = self.shadow.add(now_s, latency_100ns) else {
            return;
        };
        let stalls = self.controller.stalls();
        if stalls != self.stalls_seen {
            self.stalls_seen = stalls;
            return;
        }
        self.latency_ms = mean_100ns as f64 / 10_000.0;
        let slew_ms = -self.controller.offset_ms();
        let rate = self.controller.rate_ppm();
        // The room the card's rate leaves on the side the slew works: a
        // positive error asks for more output, up to +300 (review round 2).
        let room = if slew_ms.is_sign_positive() {
            MAX_PPM - rate
        } else {
            MAX_PPM + rate
        };
        self.slew_eta_s = slew_eta_s(slew_ms, self.controller.ppm() - rate, room, self.calm_ms);
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
