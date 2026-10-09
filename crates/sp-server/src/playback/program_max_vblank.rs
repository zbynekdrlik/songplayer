//! Each `SP-program-MAX` send at a fixed point of the wall's refresh (#223
//! follow-up, 9.10.2026).
//!
//! Resolume Arena renders at the refresh of the wall (60.000 Hz on SNV) and
//! takes Spout's shared texture at its own instant in each refresh. A send
//! paced on SongPlayer's genlock grid (`program_max_send::MAX_SEND_LEAD`
//! after the offer) drifts against that refresh by a few ppm, so for
//! minutes at a time it lands next to Arena's instant and some pictures
//! show for one refresh instead of two (measured on the wall: bursts of
//! single-refresh pictures, none missed by Arena). An ordinary Spout sender
//! renders in the display's rhythm; MAX now does the same:
//!
//! - `sp_gpu::VblankTracker` measures the wall output's refresh grid
//!   ([`VblankSource`]);
//! - each boundary is sent in a slot of that grid, `vblank + phase` (the
//!   setting `program_max_vblank_phase_ms`, `sp_core::config`);
//! - [`VblankPacer`] keeps each boundary's slot in the SAME relation to its
//!   offer as the last one's while that stays inside the lead window
//!   [`LEAD_MIN`] … `LEAD_MIN` + one period + [`LEAD_HYSTERESIS`], so the
//!   30 fps boundaries land on every second refresh. Only when the slow
//!   drift between the two clocks carries the lead out of the window is a
//!   new slot picked (a `slot_repick`: one picture held one refresh more or
//!   less, about once per drift cycle, hours); the hysteresis keeps an
//!   offer's jitter at the window's edge from picking back and forth.
//!
//! No grid (no tracker, or its output stalled) → the constant lead
//! (`program_max_send::send_due`).

use std::time::{Duration, Instant};

use sp_gpu::VblankGrid;

use crate::playback::program_max_send::send_due;

/// The earliest a boundary is sent after its offer: the constant lead the
/// compose fits in (`program_max_send::MAX_SEND_LEAD`).
pub const LEAD_MIN: Duration = Duration::from_millis(12);

/// How far past one period the lead may grow before a new slot is picked:
/// above an offer's jitter (p99 0.19 ms on SNV), so the pick never flaps.
pub const LEAD_HYSTERESIS: Duration = Duration::from_millis(3);

/// The wall's refresh, as the `program-max` thread reads it.
pub trait VblankSource {
    /// The grid at `now`, or `None` while it is not measured.
    fn grid(&self, now: Instant) -> Option<VblankGrid>;
    /// The output it measures (the telemetry's `vblank_output`).
    fn output(&self) -> String;
}

impl VblankSource for sp_gpu::VblankTracker {
    /// `mutants::skip`: one call; off Windows no tracker exists to call it
    /// on (`sp-gpu`'s stub is uninhabited).
    #[cfg_attr(test, mutants::skip)]
    fn grid(&self, now: Instant) -> Option<VblankGrid> {
        sp_gpu::VblankTracker::grid(self, now)
    }

    /// `mutants::skip`: as `grid`.
    #[cfg_attr(test, mutants::skip)]
    fn output(&self) -> String {
        sp_gpu::VblankTracker::output(self).label()
    }
}

/// How a boundary on the grid went out (the telemetry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aligned {
    /// The grid's period.
    pub period: Duration,
    /// Where after the vblank the send started, µs.
    pub phase_us: u64,
    /// Its slot was picked anew ([`Due::repicked`]).
    pub repicked: bool,
}

/// When a boundary is sent ([`VblankPacer::due`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Due {
    pub at: Instant,
    /// The last boundary's lead left its window: a new slot was picked.
    pub repicked: bool,
}

/// `t` − `origin`, ns, signed (both measured from the earlier one, so no
/// branch has an equal-instant edge).
fn signed_ns(t: Instant, origin: Instant) -> i128 {
    let earlier = t.min(origin);
    (t - earlier).as_nanos() as i128 - (origin - earlier).as_nanos() as i128
}

/// `at` moved by `ns` (signed).
fn shifted(at: Instant, ns: i128) -> Instant {
    let nanos = |n: i128| Duration::from_nanos(u64::try_from(n.max(0)).unwrap_or(u64::MAX));
    (at + nanos(ns)).checked_sub(nanos(-ns)).unwrap_or(at)
}

/// A grid's slots: `grid.at + phase + k·period`, k any integer.
struct Slots {
    origin: Instant,
    period: i128,
}

impl Slots {
    fn new(grid: &VblankGrid, phase: Duration) -> Self {
        Self {
            origin: grid.at + phase,
            period: (grid.period.as_nanos() as i128).max(1),
        }
    }

    /// The slot nearest `t` (a tie: the later one).
    fn nearest(&self, t: Instant) -> Instant {
        let offset = signed_ns(t, self.origin);
        let k = (2 * offset + self.period).div_euclid(2 * self.period);
        shifted(self.origin, k * self.period)
    }

    /// The first slot at or after `t`.
    fn at_or_after(&self, t: Instant) -> Instant {
        let offset = signed_ns(t, self.origin);
        let k = -((-offset).div_euclid(self.period));
        shifted(self.origin, k * self.period)
    }
}

/// Where `t` lies after the vblank before it: 0 ≤ … < the period.
pub fn phase_after_vblank(grid: &VblankGrid, t: Instant) -> Duration {
    let period = (grid.period.as_nanos() as i128).max(1);
    let phase = signed_ns(t, grid.at).rem_euclid(period);
    Duration::from_nanos(u64::try_from(phase).unwrap_or(0))
}

/// The slot each boundary is sent in; it remembers the last one's lead.
#[derive(Debug, Default)]
pub struct VblankPacer {
    /// The last boundary on the grid: its due − its offer.
    lead: Option<Duration>,
}

impl VblankPacer {
    /// When the boundary offered at `offered` is sent: on `grid` (phase
    /// `phase` after each vblank), in the slot nearest the last lead while
    /// that lies in the lead window, else the first slot from [`LEAD_MIN`]
    /// on; with no grid, at the constant lead (and the next grid starts
    /// afresh).
    pub fn due(&mut self, offered: Instant, grid: Option<VblankGrid>, phase: Duration) -> Due {
        let Some(grid) = grid else {
            self.lead = None;
            return Due {
                at: send_due(offered),
                repicked: false,
            };
        };
        let slots = Slots::new(&grid, phase);
        let earliest = offered + LEAD_MIN;
        let latest = earliest + grid.period + LEAD_HYSTERESIS;
        if let Some(lead) = self.lead {
            let kept = slots.nearest(offered + lead);
            if kept >= earliest && kept <= latest {
                self.lead = Some(kept - offered);
                return Due {
                    at: kept,
                    repicked: false,
                };
            }
        }
        let at = slots.at_or_after(earliest);
        let repicked = self.lead.is_some();
        self.lead = Some(at - offered);
        Due { at, repicked }
    }
}

#[cfg(test)]
#[path = "program_max_vblank_tests.rs"]
mod tests;
