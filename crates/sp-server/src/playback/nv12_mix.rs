//! The fused, row-banded NV12 paint of one `SP-program` picture (#215
//! addendum 3, design record 5858472395; #223 follow-up, design record
//! 5973498519). A child of `program_transition`, so it reads [`FitPlan`]'s
//! private rectangle and taps.
//!
//! Box run 2 (#215 comment 5858465542) measured the fitted dissolve at ~56 ms
//! per 2560×1440 boundary on the `SP-program` thread: the black canvas, the
//! bilinear fit into a scratch buffer, then the blend — three full passes on
//! one core. Addendum 3 fused the outgoing side's fit with the blend. #223
//! then made every program picture the 1920×1080 canvas, and a fade became
//! two passes again: the incoming side fitted into the canvas, then the
//! outgoing side fitted and blended over it (10.7–30.7 ms per boundary on
//! the box, #223 comment 5973492929).
//!
//! [`mix_nv12_into`] paints one destination picture (the canvas) from a
//! [`Paint`]: a plain fit of ONE side, or a fade boundary of TWO sides
//! blended at the boundary's Q8 weight. Each side is read in the destination
//! layout as it is painted ([`Side`]): the studio black, a picture already
//! in that layout (byte for byte), or a picture fitted into it by its own
//! [`FitPlan`]. Every destination byte is written in ONE pass, a row at a
//! time: the outgoing side's bytes first, then the incoming side's blended
//! over them while the row is still in cache,
//! `(f·(256 − w) + t·w + 128) >> 8`. The fitted bytes are never stored.
//!
//! The result is bit-identical to the two-pass reference: each side made a
//! whole destination picture first (`FitPlan::apply`, its own bytes, or
//! `black_nv12_into`), then `blend_nv12_into` (the same taps, the same Q8
//! rounding); `nv12_mix_tests.rs` pins that. A plain fit is the outgoing
//! side alone, which is what the blend gives at weight 0.
//!
//! The rows are shared out in K contiguous bands (`band_bounds`): band `i`
//! paints the luma rows `dh · i / K .. dh · (i + 1) / K` and the chroma rows
//! `ch · i / K .. ch · (i + 1) / K`, and the last band also paints every byte
//! past the chroma plane. K is the [`BandPool`]'s: band 0 runs on the calling
//! thread, band `i` on the pool's persistent worker `i` (`band_pool.rs`, #223
//! follow-up: no thread is started per picture). Each byte is written by
//! exactly one band, and every band uses the same pure per-byte rule. So the
//! picture does not depend on K, or on where a run starts. The `SP-program`
//! sender's pool has K = [`mix_bands`] of the box's logical processors: a
//! quarter of them, at most [`MAX_MIX_BANDS`].

use std::sync::{Mutex, PoisonError};

use super::{FitPlan, Layout, Q8_ONE, bilinear, nv12_whole, tap};
use crate::playback::band_pool::BandPool;

/// The most row bands (threads) one picture is painted in.
pub const MAX_MIX_BANDS: usize = 6;

/// One band per this many logical processors, so a picture never takes more
/// than a quarter of the box from the paced senders, OBS and Arena.
pub const CPUS_PER_MIX_BAND: usize = 4;

/// The name of the `SP-program` sender's band workers: `program-mix-1` …
pub const MIX_THREAD_NAME: &str = "program-mix";

/// How many row bands the `SP-program` sender paints a picture in on a box
/// with `logical_cpus` logical processors: a quarter of them, at least 1, at
/// most [`MAX_MIX_BANDS`] (the 24-thread box: 6).
pub fn mix_bands(logical_cpus: usize) -> usize {
    (logical_cpus / CPUS_PER_MIX_BAND).clamp(1, MAX_MIX_BANDS)
}

/// One side of a picture [`mix_nv12_into`] paints, read in the destination
/// layout as it is painted.
#[derive(Clone, Copy, Debug)]
pub enum Side<'a> {
    /// The destination's studio black: Y 16 over the luma plane, UV 128
    /// after it (`black_nv12_into`). Drawn, never read (#223: a fade's
    /// missing side, the canvas black).
    Black,
    /// A picture already in the destination layout (#223: a canvas picture),
    /// read byte for byte. A paint is never longer than it.
    Same(&'a [u8]),
    /// A picture of the plan's source layout, fitted into the destination as
    /// it is read. The plan's destination must be the paint's.
    Fitted(&'a FitPlan, &'a [u8]),
}

/// What one destination picture is painted from.
#[derive(Clone, Copy, Debug)]
pub enum Paint<'a> {
    /// A plain fit: ONE side, every byte its own.
    Fit(Side<'a>),
    /// A fade boundary: every byte `blend(from, to, weight)`, `weight` the
    /// incoming side's Q8 weight (capped at all-`to`).
    Fade {
        from: Side<'a>,
        to: Side<'a>,
        weight: u32,
    },
}

/// Paint `paint` into `out` (appended) as a `dst` picture, in the `pool`'s
/// row bands: every byte computed once. Bit-identical to the two-pass
/// reference (each side made a whole `dst` picture, then
/// `blend_nv12_into`), and to any other band count. As long as `dst`, or as
/// a [`Side::Same`] picture when that is shorter (the bytes every side
/// holds). Panics when a [`Side::Fitted`] plan does not fit into `dst`.
///
/// Returns how many threads painted it: the calling one and every worker
/// that took its band — one per band, fewer only when a worker could not
/// start and its band was painted on the calling thread.
pub fn mix_nv12_into(dst: Layout, paint: Paint<'_>, pool: &BandPool, out: &mut Vec<u8>) -> usize {
    let mix = Mix::new(dst, paint);
    let len = mix.len();
    let k = pool.bands();
    let base = out.len();
    // A memset of the pooled buffer: the bands then need disjoint `&mut`
    // slices of it (safe code), and every byte is overwritten once.
    out.resize(base + len, 0);
    let slots: Vec<Mutex<Option<Band<'_>>>> =
        split_bands(&mut out[base..], &band_bounds(dst, len, k), k)
            .into_iter()
            .map(|band| Mutex::new(Some(band)))
            .collect();
    pool.run(&|band: usize| paint_slot(&mix, &slots[band]))
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

/// Paint the band a slot still holds (each slot is painted once).
fn paint_slot(mix: &Mix<'_>, slot: &Mutex<Option<Band<'_>>>) {
    let band = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(band) = band {
        band.paint(mix);
    }
}

/// How a side's byte lands on the destination byte under it.
trait Put: Copy {
    /// One byte.
    fn put(self, out: &mut u8, byte: u8);
    /// One byte over a whole run (a bar, the black).
    fn fill(self, out: &mut [u8], byte: u8);
    /// A run of bytes (a picture already in the destination layout).
    fn copy(self, out: &mut [u8], bytes: &[u8]);
}

/// The outgoing side, and a plain fit's one side: its bytes as they are.
#[derive(Clone, Copy, Debug)]
struct Write;

impl Put for Write {
    fn put(self, out: &mut u8, byte: u8) {
        *out = byte;
    }

    fn fill(self, out: &mut [u8], byte: u8) {
        out.fill(byte);
    }

    fn copy(self, out: &mut [u8], bytes: &[u8]) {
        out.copy_from_slice(bytes);
    }
}

/// The Q8 blend of one byte, `(f·(256 − w) + t·w + 128) >> 8`: the rounding
/// of `blend_nv12_into`, the weight capped at all-`to`. As a [`Put`] it is
/// the incoming side: its byte `t` blended over the outgoing byte `f`
/// already painted under it.
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
}

impl Put for Weight {
    fn put(self, out: &mut u8, byte: u8) {
        *out = self.mix(*out, byte);
    }

    fn fill(self, out: &mut [u8], byte: u8) {
        for o in out {
            *o = self.mix(*o, byte);
        }
    }

    fn copy(self, out: &mut [u8], bytes: &[u8]) {
        for (o, &b) in out.iter_mut().zip(bytes) {
            *o = self.mix(*o, b);
        }
    }
}

impl Side<'_> {
    /// The bytes of a `len`-byte paint this side can give: all of them, or
    /// a [`Side::Same`] picture's own when it is shorter.
    fn holds(self, len: usize) -> usize {
        match self {
            Side::Same(picture) => len.min(picture.len()),
            _ => len,
        }
    }

    /// Paint one run of one destination row with this side's bytes,
    /// landing as `put` says.
    fn paint_row<P: Put>(self, span: RowSpan, out: &mut [u8], put: P) {
        match self {
            Side::Black => put.fill(out, span.plane.black()),
            Side::Same(picture) => put.copy(out, &picture[span.at..span.at + out.len()]),
            Side::Fitted(plan, src) => plan.paint_row(src, span, out, put),
        }
    }
}

/// A plane of the destination picture.
#[derive(Clone, Copy, Debug)]
enum Plane {
    /// The luma plane.
    Luma,
    /// The interleaved chroma plane, and every byte past it.
    Chroma,
}

impl Plane {
    /// Its studio black: Y 16, UV 128 (`black_nv12_into`).
    fn black(self) -> u8 {
        match self {
            Plane::Luma => 16,
            Plane::Chroma => 128,
        }
    }
}

/// Where one run of one destination row is: its plane, its row in that
/// plane, its first column, and its first byte in the picture.
#[derive(Clone, Copy, Debug)]
struct RowSpan {
    plane: Plane,
    row: usize,
    c0: usize,
    at: usize,
}

/// What every band reads: the destination layout, the outgoing side (a
/// plain fit's only side), and the incoming side with its weight.
struct Mix<'a> {
    dst: Layout,
    from: Side<'a>,
    over: Option<(Side<'a>, Weight)>,
}

impl<'a> Mix<'a> {
    /// The mix of `paint` into a `dst` picture. Panics when a fitted side's
    /// plan fits into another layout: its rows would not be `dst`'s.
    fn new(dst: Layout, paint: Paint<'a>) -> Self {
        let (from, over) = match paint {
            Paint::Fit(side) => (side, None),
            Paint::Fade { from, to, weight } => (from, Some((to, Weight::new(weight)))),
        };
        for side in [Some(from), over.map(|(to, _)| to)].into_iter().flatten() {
            if let Side::Fitted(plan, _) = side {
                assert_eq!(
                    plan.dst, dst,
                    "a fitted side's plan fits into the paint's layout"
                );
            }
        }
        Self { dst, from, over }
    }

    /// The bytes it paints: the destination's, or fewer when a
    /// [`Side::Same`] picture is shorter.
    fn len(&self) -> usize {
        let len = self.from.holds(self.dst.len);
        self.over.map_or(len, |(to, _)| to.holds(len))
    }

    /// Paint the destination bytes `[at, at + out.len())`, wherever the run
    /// starts: it is cut at the plane edge, and each part is walked row by
    /// row ([`Mix::plane_run`]).
    fn paint(&self, at: usize, out: &mut [u8]) {
        let luma_end = self.dst.stride as usize * self.dst.height as usize;
        let split = luma_end.saturating_sub(at).min(out.len());
        let (luma, chroma) = out.split_at_mut(split);
        self.plane_run(Plane::Luma, at, at, luma);
        // The chroma part (it starts on the plane edge when the run crosses
        // it) also holds the bytes past the chroma plane: rows below the
        // picture, all bar.
        let offset = at.max(luma_end) - luma_end;
        self.plane_run(Plane::Chroma, offset, at + split, chroma);
    }

    /// The part of a run inside one plane, from `offset` bytes into the
    /// plane (byte `at` of the picture): the rest of its first row, then
    /// whole `stride`-byte rows (the last one maybe cut short), each painted
    /// from both sides ([`Mix::row`]). The rows are chunks of the run, so
    /// every step paints at least one byte. (A zero stride, a layout with no
    /// rows to draw in, is walked a byte at a time; a fitted side paints it
    /// black.)
    fn plane_run(&self, plane: Plane, offset: usize, at: usize, out: &mut [u8]) {
        let ds = (self.dst.stride as usize).max(1);
        let (row, c0) = (offset / ds, offset % ds);
        let head = (ds - c0).min(out.len());
        let (first, rest) = out.split_at_mut(head);
        self.row(RowSpan { plane, row, c0, at }, first);
        for (i, run) in rest.chunks_mut(ds).enumerate() {
            let at = at + head + i * ds;
            let row = row + 1 + i;
            self.row(
                RowSpan {
                    plane,
                    row,
                    c0: 0,
                    at,
                },
                run,
            );
        }
    }

    /// One run of one row, while it is in cache: the outgoing side's bytes,
    /// then the incoming side's blended over them.
    fn row(&self, span: RowSpan, out: &mut [u8]) {
        self.from.paint_row(span, out, Write);
        if let Some((to, weight)) = self.over {
            to.paint_row(span, out, weight);
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
    /// test) and a destination with rows to draw in; otherwise the fitted
    /// picture is the studio black alone.
    fn draws(&self, src: &[u8]) -> bool {
        nv12_whole(self.src, src.len()) && nv12_whole(self.dst, self.dst.len) && self.dst.stride > 0
    }

    /// Paint one run of one destination row of the fitted picture, landing
    /// as `put` says: inside the fitted rectangle the bilinear sample,
    /// outside it the bar's studio black (Y 16 in the luma plane, UV 128
    /// after it). A picture that is not whole NV12 is the black alone.
    fn paint_row<P: Put>(&self, src: &[u8], span: RowSpan, out: &mut [u8], put: P) {
        if !self.draws(src) {
            put.fill(out, span.plane.black());
            return;
        }
        match span.plane {
            Plane::Luma => self.luma_run(src, span.row, span.c0, out, put),
            Plane::Chroma => self.chroma_run(src, span.row, span.c0, out, put),
        }
    }

    /// One run of luma row `row` from column `c0`: the bar's Y 16, the
    /// bilinear sample inside the fitted rectangle (`apply`'s luma loop).
    fn luma_run<P: Put>(&self, src: &[u8], row: usize, c0: usize, out: &mut [u8], put: P) {
        let y = row
            .checked_sub(self.y0)
            .filter(|&y| y < self.height as usize);
        let (a, b, x) = match y {
            Some(_) => span(c0, out.len(), self.x0, self.width as usize),
            None => (0, 0, 0),
        };
        put.fill(&mut out[..a], 16);
        if let Some(y) = y {
            let ty = tap(y as u32, self.height, self.src.height);
            let ss = self.src.stride as usize;
            let (r0, r1) = (&src[ty.i0 * ss..], &src[ty.i1 * ss..]);
            for (o, c) in out[a..b].iter_mut().zip(self.luma.iter().skip(x)) {
                put.put(
                    o,
                    bilinear(r0[c.i0], r0[c.i1], r1[c.i0], r1[c.i1], c.w, ty.w),
                );
            }
        }
        put.fill(&mut out[b..], 16);
    }

    /// One run of chroma row `row` (from the chroma plane's start) from byte
    /// column `c0`: the bar's UV 128, the bilinear sample of each interleaved
    /// U and V byte inside the fitted rectangle (`apply`'s chroma loop: its
    /// whole pairs of bytes, half its rows).
    fn chroma_run<P: Put>(&self, src: &[u8], row: usize, c0: usize, out: &mut [u8], put: P) {
        let y = row
            .checked_sub(self.y0 / 2)
            .filter(|&y| y < self.height as usize / 2);
        let (a, b, x) = match y {
            Some(_) => span(c0, out.len(), self.x0, 2 * (self.width as usize / 2)),
            None => (0, 0, 0),
        };
        put.fill(&mut out[..a], 128);
        if let Some(y) = y {
            let ty = tap(y as u32, self.height / 2, self.src.height.div_ceil(2));
            let ss = self.src.stride as usize;
            let s_uv = ss * self.src.height as usize;
            let (r0, r1) = (&src[s_uv + ty.i0 * ss..], &src[s_uv + ty.i1 * ss..]);
            for (i, o) in out[a..b].iter_mut().enumerate() {
                let j = x + i;
                let c = &self.chroma[j / 2];
                let (p, q) = (2 * c.i0 + j % 2, 2 * c.i1 + j % 2);
                put.put(o, bilinear(r0[p], r0[q], r1[p], r1[q], c.w, ty.w));
            }
        }
        put.fill(&mut out[b..], 128);
    }
}

#[cfg(test)]
#[path = "nv12_mix_tests.rs"]
mod tests;
