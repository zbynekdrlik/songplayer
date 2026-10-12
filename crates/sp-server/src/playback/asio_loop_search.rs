//! #233 (the SoundGrid loop check, `asio_loop.rs`): where the sent program
//! returns on the driver's inputs — which input carries the sent L and
//! which the sent R, the round trip to the exact frame, and each one's
//! least-squares gain (its polarity included). Pure.
//!
//! The window is the last [`WINDOW`] returned frames; the sent side is that
//! window + `max_lag` frames long, so a lag `d` compares the return with the
//! sent frames `d` earlier. Coarse to fine, so the CPU is bounded:
//!
//! 1. coarse: both sides summed over [`DECIMATE`] frames (a box filter,
//!    decimated), every input against the sent L and R at every lag that is
//!    a multiple of [`DECIMATE`], by normalized correlation; the best
//!    [`CANDIDATES`] lags kept, one per [`REFINE_FRAMES`] neighbourhood;
//! 2. refine: each candidate's input and channel at every frame within
//!    [`REFINE_FRAMES`] of it, at full rate; the best one is the round trip;
//! 3. assign: at that round trip, each sent channel takes the input it
//!    correlates with best and that input's least-squares gain, only when
//!    the window passes the judge's own test (`asio_loop::verdict`: the
//!    residual 40 dB under what was expected), so a lock never starts on a
//!    mismatch.
//!
//! Only a lag whose sent window has energy (over −50 dBFS) is looked at.
//! The work, in multiply-adds of the correlation loops (`work`): coarse
//! 2 · inputs · (max_lag / 16 + 1) · W / 16, refine at most 4 · 65 · W,
//! assign 2 · 2 · inputs · W. At 96 kHz with 8 inputs (W = 8192, a 0.5 s
//! range): ≤ 24.6 M + 2.1 M + 0.3 M ≈ 27 M per attempt; the analyzer makes
//! one every 0.5 s of returned audio while it searches, none while locked.

use super::{Verdict, has_energy, verdict};

/// The coarse step: frames summed into one coarse sample.
pub const DECIMATE: usize = 16;
/// The returned frames one attempt correlates.
pub const WINDOW: usize = 8192;
/// The coarse lags refined at full rate.
pub const CANDIDATES: usize = 4;
/// How far from a coarse lag the refine looks, frames (two coarse steps).
pub const REFINE_FRAMES: usize = 32;

/// Where one sent channel returns: the input and its gain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tap {
    pub input: usize,
    pub gain: f64,
}

/// A search's answer: the round trip (frames) and each sent channel's tap
/// (L, R; `None` = no input carries it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Found {
    pub delay: usize,
    pub taps: [Option<Tap>; 2],
}

/// A coarse lag worth refining.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub score: f64,
    pub lag: usize,
    pub input: usize,
    pub channel: usize,
}

/// Prefix sums of the squares: `p[n]` = Σ `x[..n]`².
pub fn prefix_squares(x: &[f32]) -> Vec<f64> {
    let mut p = Vec::with_capacity(x.len() + 1);
    let mut acc = 0.0;
    p.push(acc);
    for &v in x {
        acc += f64::from(v) * f64::from(v);
        p.push(acc);
    }
    p
}

/// Σ `x[from..from + len]`², from `x`'s prefix sums `p`.
pub fn energy(p: &[f64], from: usize, len: usize) -> f64 {
    p[from + len] - p[from]
}

/// `x` summed over every [`DECIMATE`] frames (a trailing part dropped).
pub fn box_sums(x: &[f32]) -> Vec<f64> {
    x.chunks_exact(DECIMATE)
        .map(|b| b.iter().map(|&v| f64::from(v)).sum::<f64>())
        .collect()
}

/// Σ a·b in f64.
pub fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| f64::from(x) * f64::from(y))
        .sum()
}

/// Σ a·b of two coarse sequences.
pub fn dot64(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// |ab| / √(aa · bb): the normalized correlation's size, 0 for a silent
/// side.
pub fn correlation(ab: f64, aa: f64, bb: f64) -> f64 {
    (ab / (aa * bb).sqrt().max(f64::MIN_POSITIVE)).abs()
}

/// Keep `cand` among the best [`CANDIDATES`] lags: one within
/// [`REFINE_FRAMES`] of a kept one only replaces it when it is better (the
/// refine covers its neighbourhood); else it takes a free place, or the
/// weakest one's when it is better.
pub fn offer(top: &mut Vec<Candidate>, cand: Candidate) {
    if let Some(near) = top
        .iter_mut()
        .find(|t| t.lag.abs_diff(cand.lag) <= REFINE_FRAMES)
    {
        if cand.score > near.score {
            *near = cand;
        }
        return;
    }
    if top.len() < CANDIDATES {
        top.push(cand);
        return;
    }
    if let Some(weakest) = top.iter_mut().min_by(|a, b| a.score.total_cmp(&b.score))
        && cand.score > weakest.score
    {
        *weakest = cand;
    }
}

/// Where `sent` (L, R: [`WINDOW`] + `max_lag` frames each) returns on
/// `recv` (one slice per input, the last [`WINDOW`] frames): see the module
/// doc. `None` when nothing returns as sent, or for a window of any other
/// shape (`max_lag` a multiple of [`DECIMATE`]). `work` counts the
/// multiply-adds of the correlation loops.
pub fn search(sent: [&[f32]; 2], recv: &[&[f32]], max_lag: usize, work: &mut u64) -> Option<Found> {
    let shaped = !recv.is_empty()
        && max_lag.is_multiple_of(DECIMATE)
        && recv.iter().all(|r| r.len() == WINDOW)
        && sent.iter().all(|s| s.len() == WINDOW + max_lag);
    if !shaped {
        return None;
    }
    let squares = sent.map(prefix_squares);
    // The sent window at `lag` has energy to search by.
    let loud = |channel: usize, lag: usize| {
        has_energy(energy(&squares[channel], max_lag - lag, WINDOW), WINDOW)
    };
    let top = coarse(sent, recv, max_lag, &loud, work);
    let mut best: Option<(f64, usize)> = None;
    for cand in &top {
        let r = recv[cand.input];
        let rr = dot(r, r);
        let lo = cand.lag.saturating_sub(REFINE_FRAMES);
        let hi = (cand.lag + REFINE_FRAMES).min(max_lag);
        for lag in (lo..=hi).filter(|&lag| loud(cand.channel, lag)) {
            let at = max_lag - lag;
            *work += WINDOW as u64;
            let ab = dot(r, &sent[cand.channel][at..at + WINDOW]);
            let score = correlation(ab, rr, energy(&squares[cand.channel], at, WINDOW));
            if best.is_none_or(|(b, _)| score > b) {
                best = Some((score, lag));
            }
        }
    }
    let (_, delay) = best?;
    let taps = sent.map(|x| tap_at(&x[max_lag - delay..][..WINDOW], recv, work));
    taps.iter()
        .any(Option::is_some)
        .then_some(Found { delay, taps })
}

/// The coarse pass: the best [`CANDIDATES`] lags over every input and
/// sent channel (`loud`: the sent window at a lag has energy).
fn coarse(
    sent: [&[f32]; 2],
    recv: &[&[f32]],
    max_lag: usize,
    loud: &dyn Fn(usize, usize) -> bool,
    work: &mut u64,
) -> Vec<Candidate> {
    let xs = sent.map(box_sums);
    let steps = max_lag / DECIMATE;
    let mut top = Vec::with_capacity(CANDIDATES);
    for (input, r) in recv.iter().enumerate() {
        let y = box_sums(r);
        let yy = dot64(&y, &y);
        for (channel, x) in xs.iter().enumerate() {
            for step in 0..=steps {
                let lag = step * DECIMATE;
                if !loud(channel, lag) {
                    continue;
                }
                let window = &x[steps - step..][..y.len()];
                *work += y.len() as u64;
                let score = correlation(dot64(&y, window), yy, dot64(window, window));
                offer(
                    &mut top,
                    Candidate {
                        score,
                        lag,
                        input,
                        channel,
                    },
                );
            }
        }
    }
    top
}

/// The input that carries the sent window `x` best, and its least-squares
/// gain — only when the window passes the judge's own test (`verdict`
/// reads it [`Verdict::Ok`]); `None` for a silent `x`.
fn tap_at(x: &[f32], recv: &[&[f32]], work: &mut u64) -> Option<Tap> {
    let xx = dot(x, x);
    if !has_energy(xx, x.len()) {
        return None;
    }
    let mut best: Option<(f64, usize, f64, f64)> = None;
    for (input, r) in recv.iter().enumerate() {
        *work += 2 * x.len() as u64;
        let (xy, yy) = (dot(r, x), dot(r, r));
        let fit = correlation(xy, xx, yy);
        if best.is_none_or(|(b, ..)| fit > b) {
            best = Some((fit, input, xy, yy));
        }
    }
    let (_, input, xy, yy) = best?;
    let gain = xy / xx;
    // With the fitted gain the residual is the return less what the sent
    // window explains (yy − xy² / xx).
    let fits = verdict(x.len(), xx, yy, yy - xy * gain, gain) == Verdict::Ok;
    fits.then_some(Tap { input, gain })
}

#[cfg(test)]
#[path = "asio_loop_search_tests.rs"]
mod tests;
