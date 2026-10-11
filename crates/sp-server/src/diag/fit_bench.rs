//! #223 S10a: the program canvas fit bench behind `POST /api/v1/diag/fit-bench`.
//!
//! SP-program sends every picture in its 1920×1080 canvas: a source picture
//! of another size is fitted into it on the sender's thread at every boundary
//! (`program_canvas::Canvas::fit`, the fused kernel in row bands on a
//! `BandPool`), and a fade paints both sides in one pass (`Canvas::fade`).
//! With 4K downloads (S10b) the on-air source is 3840×2160: the bench
//! measures what that costs on the box before the default moves (G3).
//!
//! It fits a synthetic NV12 picture of the asked size `frames` times, then
//! fades it into itself `frames` times, on a pool of the sender's band count
//! (`mix_bands` of the box's logical processors) built for the run. Its
//! workers run at normal priority while SongPlayer plays as usual, so the
//! numbers are an upper bound of the sender's own. The gate: p99 at most half
//! of a 30 fps slot.

use std::time::Instant;

use serde::Serialize;
use sp_core::nv12::nv12_len;

use super::decode_bench::DecodeUs;
use crate::playback::band_pool::BandPool;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_canvas::Canvas;
use crate::playback::program_output::{PROGRAM_STANDBY_H, PROGRAM_STANDBY_W};
use crate::playback::program_transition::{Layout, Q8_ONE};

/// The largest picture the bench takes (the 4K downloads of S10b).
pub const MAX_WIDTH: u32 = 3840;
pub const MAX_HEIGHT: u32 = 2160;
/// The most fits (and fades) one run makes.
pub const MAX_FRAMES: u32 = 600;
/// G3's bound: half of a 30 fps slot, µs.
pub const BUDGET_US: u64 = 16_666;

/// The bench's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FitBenchReport {
    pub width: u32,
    pub height: u32,
    pub canvas_width: u32,
    pub canvas_height: u32,
    /// The row bands each picture is painted in.
    pub bands: usize,
    pub frames: u32,
    /// One fit into the canvas.
    pub fit_us: DecodeUs,
    /// One fade of the picture into itself (both sides fitted).
    pub fade_us: DecodeUs,
    pub budget_us: u64,
    /// G3 fails: a p99 over [`BUDGET_US`].
    pub over_budget: bool,
}

/// Whether a request's size and count can run: a size of even sides from
/// 2×2 to [`MAX_WIDTH`]×[`MAX_HEIGHT`] (NV12's chroma is half each side), and
/// 1 to [`MAX_FRAMES`] frames.
pub fn check(width: u32, height: u32, frames: u32) -> Result<(), &'static str> {
    if !(2..=MAX_WIDTH).contains(&width) || !(2..=MAX_HEIGHT).contains(&height) {
        return Err("width must be 2..=3840 and height 2..=2160");
    }
    if !(width.is_multiple_of(2) && height.is_multiple_of(2)) {
        return Err("width and height must be even (NV12)");
    }
    if !(1..=MAX_FRAMES).contains(&frames) {
        return Err("frames must be 1..=600");
    }
    Ok(())
}

/// A `width`×`height` NV12 picture with real content (a luma ramp along each
/// row, chroma ramps), so the kernel reads every byte it would read from a
/// decoded picture.
pub fn synthetic(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; nv12_len(width, height)];
    let (luma, chroma) = out.split_at_mut(w * h);
    for (y, row) in luma.chunks_mut(w).enumerate() {
        for (x, px) in row.iter_mut().enumerate() {
            *px = (16 + (x + y) % 220) as u8;
        }
    }
    for (i, px) in chroma.iter_mut().enumerate() {
        *px = (64 + i % 128) as u8;
    }
    out
}

/// The report of a run whose fits took `fit` µs and fades `fade` µs.
pub fn report(width: u32, height: u32, bands: usize, fit: &[u64], fade: &[u64]) -> FitBenchReport {
    let (fit_us, fade_us) = (DecodeUs::of(fit), DecodeUs::of(fade));
    FitBenchReport {
        width,
        height,
        canvas_width: PROGRAM_STANDBY_W,
        canvas_height: PROGRAM_STANDBY_H,
        bands,
        frames: u32::try_from(fit.len()).unwrap_or(u32::MAX),
        fit_us,
        fade_us,
        budget_us: BUDGET_US,
        over_budget: fit_us.p99 > BUDGET_US || fade_us.p99 > BUDGET_US,
    }
}

/// Run the bench (module doc) on the calling thread with `bands` row bands.
/// A checked size and count ([`check`]).
#[cfg_attr(test, mutants::skip)] // wall-clock timing; `check`, `synthetic` and `report` are tested
pub fn run(width: u32, height: u32, frames: u32, bands: usize) -> FitBenchReport {
    let pool = BandPool::new("fit-bench", bands);
    let mut canvas = Canvas::new(PROGRAM_STANDBY_W, PROGRAM_STANDBY_H);
    let picture = SharedFrame::new(synthetic(width, height));
    let layout = Layout {
        width,
        height,
        stride: width,
        len: picture.len(),
    };
    let micros = |t: Instant| u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mut fit = Vec::with_capacity(frames as usize);
    for _ in 0..frames {
        let t = Instant::now();
        let out = canvas.fit(layout, &picture, &pool);
        fit.push(micros(t));
        drop(out);
    }
    let mut fade = Vec::with_capacity(frames as usize);
    for _ in 0..frames {
        let side = Some((layout, &picture[..]));
        let t = Instant::now();
        let out = canvas.fade(side, side, Q8_ONE / 2, &pool);
        fade.push(micros(t));
        drop(out);
    }
    report(width, height, pool.bands(), &fit, &fade)
}

#[cfg(test)]
#[path = "fit_bench_tests.rs"]
mod tests;
