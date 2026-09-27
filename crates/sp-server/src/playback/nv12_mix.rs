//! The fused, row-banded NV12 mix of one transition boundary (#215 addendum 3,
//! design record 5858472395). A child of `program_transition`, so it reads
//! [`FitPlan`]'s private rectangle and taps.
//!
//! Box run 2 (#215 comment 5858465542) measured the fitted dissolve at ~56 ms
//! per 2560×1440 boundary on the `SP-program` thread, against a 33.3 ms slot:
//! the black canvas, the bilinear fit into a scratch buffer, then the blend —
//! three full passes over the ~5.5 MB picture, on one core.
//!
//! [`mix_nv12_into`] computes every destination byte ONCE, as
//! `blend(fit(from)[p], to[p], w)`: the fitted outgoing byte (a bar's studio
//! black outside the fitted rectangle) is never stored, it is blended with the
//! incoming byte on the spot. An outgoing picture already in the incoming
//! layout is blended byte for byte ([`Outgoing::Same`]). The result is
//! bit-identical to the two-pass reference, `FitPlan::apply` then
//! `blend_nv12_into` (the same taps, the same Q8 rounding); `nv12_mix_tests.rs`
//! pins that.
//!
//! The rows are shared out in K contiguous bands (`band_bounds`): band `i`
//! paints the luma rows `dh · i / K .. dh · (i + 1) / K` and the chroma rows
//! `ch · i / K .. ch · (i + 1) / K`, and the last band also paints every byte
//! past the chroma plane. Band 0 runs on the calling thread and every other
//! band on a scoped helper thread of its own (`std::thread::scope`, so no new
//! dependency and no thread outlives the call). Each byte is written by
//! exactly one band, and every band uses the same pure per-byte rule. So the
//! picture does not depend on K, or on where a run starts. The `SP-program`
//! sender uses K = [`mix_bands`] of the box's logical processors: a quarter of
//! them, at most [`MAX_MIX_BANDS`].

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;

use tracing::warn;

use super::{FitPlan, Layout, Q8_ONE, bilinear, nv12_whole, tap};

/// The most row bands (threads) one mixed picture is painted in.
pub const MAX_MIX_BANDS: usize = 6;

/// One band per this many logical processors, so a mix never takes more
/// than a quarter of the box from the paced senders, OBS and Arena.
pub const CPUS_PER_MIX_BAND: usize = 4;

/// The name of a band's helper thread.
const MIX_THREAD_NAME: &str = "program-mix";

/// How many row bands the `SP-program` sender paints a mixed picture in on a
/// box with `logical_cpus` logical processors: a quarter of them, at least 1,
/// at most [`MAX_MIX_BANDS`] (the 24-thread box: 6).
pub fn mix_bands(logical_cpus: usize) -> usize {
    (logical_cpus / CPUS_PER_MIX_BAND).clamp(1, MAX_MIX_BANDS)
}

/// The outgoing picture of a mixed boundary, as [`mix_nv12_into`] reads it.
#[derive(Clone, Copy, Debug)]
pub enum Outgoing<'a> {
    /// A picture already in the destination layout (the incoming side's, or
    /// the standby black made in it): blended byte for byte.
    Same(Layout, &'a [u8]),
    /// A picture of the plan's source layout, fitted into its destination
    /// (the incoming layout) as it is blended.
    Fitted(&'a FitPlan, &'a [u8]),
}

/// Mix one boundary's pictures into `out` (appended): every byte
/// `blend(fit(from)[p], to[p], weight)`, computed once, in `bands` row bands
/// (at least 1). Bit-identical to `FitPlan::apply` then `blend_nv12_into`
/// (for [`Outgoing::Same`]: to `blend_nv12_into`), and to any other band
/// count. As long as the two-pass result: the destination's bytes (the
/// `Same` picture's, the plan's destination layout's) that `to` also holds.
///
/// Returns how many threads painted it: one per band (a band with no rows
/// paints nothing), fewer only when a helper thread could not start and its
/// band was painted on the calling thread.
pub fn mix_nv12_into(
    from: Outgoing<'_>,
    to: &[u8],
    weight: u32,
    bands: usize,
    out: &mut Vec<u8>,
) -> usize {
    let (dst, len) = match from {
        Outgoing::Same(layout, picture) => (layout, picture.len()),
        Outgoing::Fitted(plan, _) => (plan.dst, plan.dst.len),
    };
    let len = len.min(to.len());
    let k = bands.max(1);
    let base = out.len();
    // A memset of the pooled buffer: the bands then need disjoint `&mut`
    // slices of it (safe code), and every byte is overwritten once.
    out.resize(base + len, 0);
    let bands = split_bands(&mut out[base..], &band_bounds(dst, len, k), k);
    let mix = Mix {
        from,
        to,
        weight: Weight::new(weight),
    };
    paint_bands(&mix, bands)
}

/// The `2k + 1` destination offsets of `k` bands over the first `len` bytes
/// of a `dst` picture. Band `i` paints `[b[i], b[i + 1])` (its share of the
/// luma rows) and `[b[k + i], b[k + i + 1])` (its share of the chroma rows;
/// the last band also every byte past them). Band `i` gets the rows
/// `rows · i / k .. rows · (i + 1) / k` of each plane, so a row count that `k`
/// does not divide spreads its extra rows over the bands, and a picture of
/// fewer rows than `k` leaves some bands empty. Every offset is capped at
/// `len`, so the bands always cover exactly `[0, len)`, whatever the layout.
fn band_bounds(dst: Layout, len: usize, k: usize) -> Vec<usize> {
    let (ds, dh) = (dst.stride as usize, dst.height as usize);
    let ch = dh.div_ceil(2);
    let luma = (0..=k).map(|i| ds * (dh * i / k));
    let chroma = (1..k).map(|i| ds * (dh + ch * i / k));
    luma.chain(chroma)
        .chain([len])
        .map(|b| b.min(len))
        .collect()
}

/// One band: its luma run and its chroma run, each with its destination
/// offset.
struct Band<'o> {
    runs: [(usize, &'o mut [u8]); 2],
}

impl Band<'_> {
    fn paint(self, mix: &Mix<'_>) {
        for (at, run) in self.runs {
            mix.paint(at, run);
        }
    }
}

/// Cut `out` at the `bounds` ([`band_bounds`]) into its `k` bands.
fn split_bands<'o>(mut out: &'o mut [u8], bounds: &[usize], k: usize) -> Vec<Band<'o>> {
    let mut runs = Vec::new();
    for edge in bounds.windows(2) {
        let (run, rest) = std::mem::take(&mut out).split_at_mut(edge[1] - edge[0]);
        runs.push((edge[0], run));
        out = rest;
    }
    let chroma = runs.split_off(k);
    runs.into_iter()
        .zip(chroma)
        .map(|(luma, chroma)| Band {
            runs: [luma, chroma],
        })
        .collect()
}

/// Paint every band: the first on the calling thread, every other one on a
/// scoped helper thread of its own, all at once. A helper that cannot start
/// has its band painted on the calling thread instead (WARN). Returns how
/// many threads painted.
fn paint_bands(mix: &Mix<'_>, bands: Vec<Band<'_>>) -> usize {
    let slots: Vec<Mutex<Option<Band<'_>>>> = bands
        .into_iter()
        .map(|band| Mutex::new(Some(band)))
        .collect();
    let helpers = AtomicUsize::new(0);
    thread::scope(|scope| {
        for slot in slots.iter().skip(1) {
            let helped = &helpers;
            let started = thread::Builder::new()
                .name(MIX_THREAD_NAME.to_owned())
                .spawn_scoped(scope, move || {
                    paint_slot(mix, slot);
                    helped.fetch_add(1, Ordering::Relaxed);
                });
            if let Err(e) = started {
                warn!(
                    %e,
                    "program transition: a mix helper thread did not start — its band is painted on the SP-program thread"
                );
                paint_slot(mix, slot);
            }
        }
        if let Some(first) = slots.first() {
            paint_slot(mix, first);
        }
    });
    1 + helpers.into_inner()
}

/// Paint the band a slot still holds (each slot is painted once).
fn paint_slot(mix: &Mix<'_>, slot: &Mutex<Option<Band<'_>>>) {
    let band = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(band) = band {
        band.paint(mix);
    }
}

/// The Q8 blend of one byte, `(f·(256 − w) + t·w + 128) >> 8`: the rounding
/// of `blend_nv12_into`, the weight capped at all-`to`.
#[derive(Clone, Copy, Debug)]
struct Weight {
    keep: u32,
    to: u32,
}

impl Weight {
    fn new(weight: u32) -> Self {
        let to = weight.min(Q8_ONE);
        Self {
            keep: Q8_ONE - to,
            to,
        }
    }

    fn mix(self, from: u8, to: u8) -> u8 {
        ((u32::from(from) * self.keep + u32::from(to) * self.to + 128) >> 8) as u8
    }

    /// One outgoing byte `from` (a bar's black) over a run of `to`.
    fn fill(self, from: u8, out: &mut [u8], to: &[u8]) {
        for (o, &t) in out.iter_mut().zip(to) {
            *o = self.mix(from, t);
        }
    }
}

/// What every band reads: both pictures and the weight.
struct Mix<'a> {
    from: Outgoing<'a>,
    to: &'a [u8],
    weight: Weight,
}

impl Mix<'_> {
    /// Paint the destination bytes `[at, at + out.len())`, wherever the run
    /// starts.
    fn paint(&self, at: usize, out: &mut [u8]) {
        let to = &self.to[at..at + out.len()];
        match self.from {
            Outgoing::Same(_, from) => {
                for ((o, &f), &t) in out.iter_mut().zip(&from[at..]).zip(to) {
                    *o = self.weight.mix(f, t);
                }
            }
            Outgoing::Fitted(plan, src) => plan.mix_run(src, at, out, to, self.weight),
        }
    }
}

/// The part of a run of `len` destination columns from column `c0` that shows
/// the picture columns `[x0, x0 + width)`: the run's `[a, b)` (empty when it
/// shows none of them), and the picture column at `a`.
fn span(c0: usize, len: usize, x0: usize, width: usize) -> (usize, usize, usize) {
    let end = c0 + len;
    let a = x0.clamp(c0, end);
    let b = (x0 + width).clamp(a, end);
    (a - c0, b - c0, c0.saturating_sub(x0))
}

impl FitPlan {
    /// Whether the fit draws its picture: both layouts whole NV12 (`apply`'s
    /// test) and a destination with rows to draw in; otherwise the outgoing
    /// picture is the black canvas alone.
    fn draws(&self, src: &[u8]) -> bool {
        nv12_whole(self.src, src.len()) && nv12_whole(self.dst, self.dst.len) && self.dst.stride > 0
    }

    /// Mix the destination bytes `[at, at + out.len())`: each one the fitted
    /// outgoing byte, blended with `to`'s. Outside the fitted rectangle that
    /// byte is the bar's studio black: Y 16 in the luma plane, UV 128 after
    /// it (`black_nv12_into`). A picture that is not whole NV12 is the black
    /// canvas alone. A run may start anywhere; it is walked row by row.
    fn mix_run(&self, src: &[u8], mut at: usize, mut out: &mut [u8], mut to: &[u8], w: Weight) {
        if !self.draws(src) {
            let luma = (self.dst.stride as usize * self.dst.height as usize).min(self.dst.len);
            let split = luma.saturating_sub(at).min(out.len());
            let (bars, rest) = out.split_at_mut(split);
            w.fill(16, bars, &to[..split]);
            w.fill(128, rest, &to[split..]);
            return;
        }
        let ds = self.dst.stride as usize;
        let luma_end = ds * self.dst.height as usize;
        while !out.is_empty() {
            let luma = at < luma_end;
            let plane = if luma { 0 } else { luma_end };
            // The chroma branch also walks the bytes past the chroma plane:
            // rows below the picture, all bar.
            let (row, c0) = ((at - plane) / ds, (at - plane) % ds);
            let len = (ds - c0).min(out.len());
            let (run, rest) = std::mem::take(&mut out).split_at_mut(len);
            let (run_to, rest_to) = to.split_at(len);
            if luma {
                self.luma_run(src, row, c0, run, run_to, w);
            } else {
                self.chroma_run(src, row, c0, run, run_to, w);
            }
            (at, out, to) = (at + len, rest, rest_to);
        }
    }

    /// One run of luma row `row` from column `c0`: the bar's Y 16, the
    /// bilinear sample inside the fitted rectangle (`apply`'s luma loop).
    fn luma_run(&self, src: &[u8], row: usize, c0: usize, out: &mut [u8], to: &[u8], w: Weight) {
        let y = row
            .checked_sub(self.y0)
            .filter(|&y| y < self.height as usize);
        let (a, b, x) = match y {
            Some(_) => span(c0, out.len(), self.x0, self.width as usize),
            None => (0, 0, 0),
        };
        w.fill(16, &mut out[..a], &to[..a]);
        if let Some(y) = y {
            let ty = tap(y as u32, self.height, self.src.height);
            let ss = self.src.stride as usize;
            let (r0, r1) = (&src[ty.i0 * ss..], &src[ty.i1 * ss..]);
            let taps = self.luma.iter().skip(x);
            for ((o, &t), c) in out[a..b].iter_mut().zip(&to[a..b]).zip(taps) {
                let f = bilinear(r0[c.i0], r0[c.i1], r1[c.i0], r1[c.i1], c.w, ty.w);
                *o = w.mix(f, t);
            }
        }
        w.fill(16, &mut out[b..], &to[b..]);
    }

    /// One run of chroma row `row` (from the chroma plane's start) from byte
    /// column `c0`: the bar's UV 128, the bilinear sample of each interleaved
    /// U and V byte inside the fitted rectangle (`apply`'s chroma loop: its
    /// whole pairs of bytes, half its rows).
    fn chroma_run(&self, src: &[u8], row: usize, c0: usize, out: &mut [u8], to: &[u8], w: Weight) {
        let y = row
            .checked_sub(self.y0 / 2)
            .filter(|&y| y < self.height as usize / 2);
        let (a, b, x) = match y {
            Some(_) => span(c0, out.len(), self.x0, 2 * (self.width as usize / 2)),
            None => (0, 0, 0),
        };
        w.fill(128, &mut out[..a], &to[..a]);
        if let Some(y) = y {
            let ty = tap(y as u32, self.height / 2, self.src.height.div_ceil(2));
            let ss = self.src.stride as usize;
            let s_uv = ss * self.src.height as usize;
            let (r0, r1) = (&src[s_uv + ty.i0 * ss..], &src[s_uv + ty.i1 * ss..]);
            for (i, (o, &t)) in out[a..b].iter_mut().zip(&to[a..b]).enumerate() {
                let j = x + i;
                let c = &self.chroma[j / 2];
                let (p, q) = (2 * c.i0 + j % 2, 2 * c.i1 + j % 2);
                let f = bilinear(r0[p], r0[q], r1[p], r1[q], c.w, ty.w);
                *o = w.mix(f, t);
            }
        }
        w.fill(128, &mut out[b..], &to[b..]);
    }
}

#[cfg(test)]
#[path = "nv12_mix_tests.rs"]
mod tests;
