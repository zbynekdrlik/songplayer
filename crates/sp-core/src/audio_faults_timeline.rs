//! #257 lane 2: the panel's timeline. A window is cut into [`BUCKETS`]
//! buckets with exact boundaries; each bucket counts its loss and risk
//! rows; a bar is drawn to scale against the panel's largest bucket (loss
//! at the bottom, risk on it); every row shares one time axis whose ticks
//! fall on whole local times. Pure; sp-ui draws the SVG.

use crate::audio_faults::{LocalTime, dm_hm_sk, losses_sk, risks_sk, time_sk};

/// Buckets per timeline, whatever the window (1 h → 1 min each, 24 h →
/// 24 min, 7 d → 2.8 h, 30 d → 12 h).
pub const BUCKETS: usize = 60;

/// A bucket's width in the SVG's units (the view box is `BUCKETS * SLOT`
/// wide).
pub const SLOT: u32 = 10;

/// A bar's width in the SVG's units (a gap of `SLOT - BAR_WIDTH`).
pub const BAR_WIDTH: u32 = 8;

/// A full bar's height in the SVG's units.
pub const BAR_HEIGHT: u32 = 24;

/// A part of a bar that is not empty is at least this tall, so one fault
/// among many is never invisible.
pub const MIN_BAR: u32 = 2;

/// The local offset's unit (ms).
const MINUTE_MS: i64 = 60_000;

/// A day (ms): a step this long or longer labels its ticks by the day.
const DAY_MS: i64 = 86_400_000;

/// `(longest window, step)`, ms: the first pair whose window holds the
/// span picks the step (≤ 2 h every 10 min, ≤ 12 h every hour, ≤ 2 d every
/// 3 h, ≤ 10 d every day). Literals: a product here would be a mutant.
const STEPS: [(i64, i64); 4] = [
    (7_200_000, 600_000),
    (43_200_000, 3_600_000),
    (172_800_000, 10_800_000),
    (864_000_000, 86_400_000),
];

/// The step of a longer window: every 5 days.
const LONG_STEP: i64 = 432_000_000;

/// No axis has more ticks than this (the loop is bounded by it).
const MAX_TICKS: i64 = 64;

/// One bucket's rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bucket {
    pub loss: u32,
    pub risk: u32,
}

/// The first instant of bucket `i` of `n` over `[from, to]`: `from + ⌈i ·
/// span / n⌉`. With these boundaries an instant `t` lies in bucket `⌊(t −
/// from) · n / span⌋` exactly ([`bucket_of`]); "bucket `n`" starts at `to`.
pub fn bucket_start(from: i64, to: i64, n: usize, i: usize) -> i64 {
    let span = u64::try_from(to - from).unwrap_or(0);
    let parts = n.max(1) as u64;
    let offset = (i as u64 * span).div_ceil(parts);
    from + offset as i64
}

/// The bucket of instant `t` (`to` itself in the last); `None` outside
/// `[from, to]` or for an empty window.
pub fn bucket_of(t: i64, from: i64, to: i64, n: usize) -> Option<usize> {
    if n == 0 || t < from || t > to || to <= from {
        return None;
    }
    let i = (t - from) as u64 * n as u64 / (to - from) as u64;
    Some((i as usize).min(n - 1))
}

/// The inner edges of `n` buckets over `[from, to]`: bucket `i`'s first
/// instant for `i` in `1..n` ([`bucket_start`]). The server's bucket query
/// numbers its rows by these very edges, so a row lies in the bucket
/// [`bucket_of`] names.
pub fn bucket_edges(from: i64, to: i64, n: usize) -> Vec<i64> {
    (1..n).map(|i| bucket_start(from, to, n, i)).collect()
}

/// A path's [`BUCKETS`] buckets from its answer's `loss_buckets` /
/// `risk_buckets` (one missing reads 0).
pub fn buckets_of(loss: &[u32], risk: &[u32]) -> Vec<Bucket> {
    (0..BUCKETS)
        .map(|i| Bucket {
            loss: loss.get(i).copied().unwrap_or(0),
            risk: risk.get(i).copied().unwrap_or(0),
        })
        .collect()
}

/// The largest bucket (loss + risk) of every row: the panel's one scale.
pub fn shared_max(rows: &[Vec<Bucket>]) -> u32 {
    rows.iter()
        .flatten()
        .map(|b| b.loss + b.risk)
        .max()
        .unwrap_or(0)
}

/// A bar's loss and risk heights, to scale against `max` (a bar of `max`
/// rows is [`BAR_HEIGHT`] tall); a part that is not empty is at least
/// [`MIN_BAR`], and the two never pass the bar's height.
pub fn bar_heights(b: Bucket, max: u32) -> (u32, u32) {
    let scaled = |count: u32| {
        if count == 0 || max == 0 {
            0
        } else {
            (count * BAR_HEIGHT / max).clamp(MIN_BAR, BAR_HEIGHT)
        }
    };
    let loss = scaled(b.loss);
    let risk = scaled(b.risk).min(BAR_HEIGHT - loss);
    (loss, risk)
}

/// Bucket `i`'s bar's left edge in the SVG's units.
pub fn bar_x(i: usize) -> u32 {
    i as u32 * SLOT + 1
}

/// A bar's tooltip: `12.10. 14:00 – 12.10. 14:24: 1 chyba, 0 rizík`.
pub fn bucket_tip(start: LocalTime, end: LocalTime, b: Bucket) -> String {
    format!(
        "{} – {}: {}, {}",
        dm_hm_sk(start),
        dm_hm_sk(end),
        losses_sk(i64::from(b.loss)),
        risks_sk(i64::from(b.risk))
    )
}

/// A path's (re)start on its timeline: a mark, never a bar (a planned
/// restart is no fault; an `interrupted` one is a loss and has its bar).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StartMark {
    /// The instant (UTC ms).
    pub at_ms: i64,
    /// Its place on the axis, percent ([`axis_pct`]).
    pub pct: f64,
}

/// The (re)starts (UTC ms, the answer's `starts`) inside `[from, to]`, as
/// marks.
pub fn start_marks(starts: &[i64], from: i64, to: i64) -> Vec<StartMark> {
    starts
        .iter()
        .filter(|t| **t >= from && **t <= to)
        .map(|t| StartMark {
            at_ms: *t,
            pct: axis_pct(*t, from, to),
        })
        .collect()
}

/// A start mark's tooltip: `spustenie 12.10. 14:05:07`.
pub fn start_tip(at: LocalTime) -> String {
    format!("spustenie {}", time_sk(at))
}

/// The step between the axis's ticks for a window `span_ms` long (4 to 12
/// ticks).
pub fn tick_step(span_ms: i64) -> i64 {
    STEPS
        .iter()
        .find(|(longest, _)| span_ms <= *longest)
        .map_or(LONG_STEP, |(_, step)| *step)
}

/// A tick of the shared time axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick {
    /// The instant (UTC ms).
    pub at_ms: i64,
    /// Labelled by the day (`12.10.`), else by the time (`14:00`).
    pub days: bool,
}

/// The ticks of the window `[from, to]`: the local instants that are whole
/// multiples of [`tick_step`] — whole 10 minutes, hours, 3 hours, local
/// midnights; for the longest windows every 5th day counted from 1.1.1970.
/// Each is snapped with the zone's offset AT that tick (`offset_at`:
/// minutes east of UTC at an instant; the browser's), so a midnight stays
/// a midnight across a daylight-saving change. A local time the spring
/// change skips maps to an instant already ticked, and the autumn's
/// repeated hour is ticked once (its summer reading): the ticks only ever
/// move forward.
pub fn axis_ticks(from: i64, to: i64, offset_at: &dyn Fn(i64) -> i32) -> Vec<Tick> {
    let step = tick_step(to - from);
    let shift = |utc: i64| i64::from(offset_at(utc)) * MINUTE_MS;
    let from_shift = shift(from);
    let local_from = from + from_shift;
    let rem = local_from.rem_euclid(step);
    let first = local_from - rem + if rem > 0 { step } else { 0 };
    let days = step >= DAY_MS;
    let mut ticks: Vec<Tick> = Vec::new();
    for local in (0..MAX_TICKS).map(|k| first + k * step) {
        let at_ms = local - from_shift;
        if at_ms > to {
            break;
        }
        if at_ms >= from && ticks.last().is_none_or(|t| at_ms > t.at_ms) {
            ticks.push(Tick { at_ms, days });
        }
    }
    ticks
}

/// An instant's place on the axis of `[from, to]`, in percent.
pub fn axis_pct(at: i64, from: i64, to: i64) -> f64 {
    if to <= from {
        return 0.0;
    }
    (at - from) as f64 * 100.0 / (to - from) as f64
}

#[cfg(test)]
#[path = "audio_faults_timeline_tests.rs"]
mod tests;
