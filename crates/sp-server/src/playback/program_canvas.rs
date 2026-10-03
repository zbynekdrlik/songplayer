//! The ONE picture layout of `SP-program` (#223): the canvas, and the fit of
//! every other picture into it, for the sender (`program_output.rs`).
//!
//! The owner's rule (ROZHODNUTÉ 28.9.2026 on #223): `SP-program` is ALWAYS
//! 1920×1080. cg OBS, the Presenter, the stage displays and the stream never
//! get a 1440p or a 4K picture, and its size never changes at a cut, a fill
//! or a fade. Before #223 it carried the on-air source's own size (2560×1440,
//! 2560×1080, 2048×858, … on the box in one evening).
//!
//! The canvas is `w`×`h` NV12 with stride `w` (`w·h·3/2` bytes). In
//! production it is 1920×1080, 3 110 400 B: `PROGRAM_STANDBY_W/H`, the size
//! of the program's standby black. Every picture the sender submits is in it:
//!
//! - a picture already in it (the canvas's size and stride, and at least its
//!   bytes) goes out as it is, the same allocation, no copy;
//! - any other picture is fitted into it ([`Canvas::fit`]). It is placed by
//!   `nv12_fit::aspect_fit` (aspect kept, centred on even offsets, bars
//!   Y 16 / UV 128) and scaled bilinear by the fused kernel
//!   (`mix_nv12_into` with `Paint::Fit(Side::Fitted)`): one side, every byte
//!   the fitted picture's, nothing else read. It runs in the sender's row
//!   bands into a `frame_pool` buffer. A larger picture is scaled down, a
//!   smaller one up, and a 16:9 one fills the canvas;
//! - a fade boundary's picture ([`Canvas::fade`], #223 follow-up, design
//!   record 5973498519) is painted in ONE pass from both sides: each one
//!   read as it is when it is a canvas picture, fitted when it is not, the
//!   canvas black when it is missing, and the incoming one blended over the
//!   outgoing one at the boundary's weight. Before, the incoming side was
//!   fitted into a canvas buffer first and blended in a second pass.
//!
//! A fit's plan (its column taps) is built once per source layout. The canvas
//! keeps the [`FIT_PLANS_KEPT`] plans used last, so a fade between two sizes,
//! which fits both sides on every boundary, rebuilds nothing; each new plan
//! is one INFO line.

use tracing::info;

use crate::playback::band_pool::BandPool;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_transition::{FitPlan, Layout, Paint, Side, mix_nv12_into};

/// How many fit plans the canvas keeps: both sides of a fade.
pub const FIT_PLANS_KEPT: usize = 2;

// A fade's picture reads both sides' plans at once (`Canvas::fade`).
const _: () = assert!(FIT_PLANS_KEPT >= 2);

/// One side of a fade boundary: a picture of its layout, or `None` when the
/// side is missing (the canvas black). The picture's own length counts,
/// whatever the layout's `len` says ([`Canvas::fade`]).
pub type FadeSide<'a> = Option<(Layout, &'a [u8])>;

/// A fade side with its layout's `len` set to its picture's own length: what
/// the canvas holds or fits is the bytes that are there.
fn measured(side: FadeSide<'_>) -> FadeSide<'_> {
    side.map(|(layout, picture)| {
        let len = picture.len();
        (Layout { len, ..layout }, picture)
    })
}

/// The program's picture layout and the fit plans into it.
#[derive(Debug)]
pub struct Canvas {
    layout: Layout,
    /// The plans kept, the one used last first.
    plans: Vec<FitPlan>,
    /// Plans built so far.
    built: u64,
}

impl Canvas {
    /// A `width`×`height` NV12 canvas, stride `width`.
    pub fn new(width: u32, height: u32) -> Self {
        let luma = width as usize * height as usize;
        Self {
            layout: Layout {
                width,
                height,
                stride: width,
                len: luma + luma / 2,
            },
            plans: Vec::new(),
            built: 0,
        }
    }

    /// The canvas's layout: every picture the program sends.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// How many fit plans were built so far.
    pub fn built(&self) -> u64 {
        self.built
    }

    /// Whether a `layout` picture already is a canvas picture: the canvas's
    /// size and stride, and at least its bytes (a decoder's buffer may carry
    /// slack past the picture). It goes out as it is.
    pub fn holds(&self, layout: Layout) -> bool {
        let canvas = self.layout;
        (layout.width, layout.height, layout.stride) == (canvas.width, canvas.height, canvas.stride)
            && layout.len >= canvas.len
    }

    /// The plan that fits a `src` picture into the canvas ([`Canvas::keep`]).
    pub fn plan(&mut self, src: Layout) -> &FitPlan {
        self.keep(src);
        &self.plans[0]
    }

    /// Keep a plan that fits a `src` picture into the canvas, as the one used
    /// last: a kept one moves to the front; otherwise a new one is built
    /// (logged), and the plan used longest ago is dropped past
    /// [`FIT_PLANS_KEPT`].
    fn keep(&mut self, src: Layout) {
        let canvas = self.layout;
        match self.plans.iter().position(|plan| plan.fits(src, canvas)) {
            Some(kept) => self.plans[..=kept].rotate_right(1),
            None => {
                self.built += 1;
                info!(
                    width = src.width,
                    height = src.height,
                    stride = src.stride,
                    canvas_width = canvas.width,
                    canvas_height = canvas.height,
                    plans_built = self.built,
                    "program output: a picture of a new size — fitted into the SP-program canvas"
                );
                self.plans.insert(0, FitPlan::new(src, canvas));
                self.plans.truncate(FIT_PLANS_KEPT);
            }
        }
    }

    /// `video`, a `layout` picture, as a canvas picture: `video` itself (the
    /// same allocation) when the canvas holds it, else fitted into the
    /// canvas — one side, nothing else read — in the `pool`'s row bands,
    /// into a pooled buffer of exactly the canvas's bytes.
    pub fn fit(&mut self, layout: Layout, video: &SharedFrame, pool: &BandPool) -> SharedFrame {
        if self.holds(layout) {
            return video.clone();
        }
        let canvas = self.layout;
        let mut out = sp_decoder::frame_pool::take(canvas.len);
        let plan = self.plan(layout);
        mix_nv12_into(
            canvas,
            Paint::Fit(Side::Fitted(plan, video)),
            pool,
            &mut out,
        );
        SharedFrame::new(out)
    }

    /// A fade boundary's picture, exactly the canvas's bytes, painted in ONE
    /// pass in the `pool`'s row bands: the incoming side `to` blended over
    /// the outgoing side `from` at the Q8 `weight`, each side as it is when
    /// the canvas holds it, fitted into the canvas when it does not, and the
    /// canvas black when it is missing. Both sides' plans are kept (the
    /// outgoing one used last). A side is judged by its picture's own
    /// length, so one shorter than its layout claims is never sent as a
    /// short canvas picture: it is fitted (and drawn black, not being whole).
    pub fn fade(
        &mut self,
        from: FadeSide<'_>,
        to: FadeSide<'_>,
        weight: u32,
        pool: &BandPool,
    ) -> SharedFrame {
        let (from, to) = (measured(from), measured(to));
        for (layout, _) in [to, from].into_iter().flatten() {
            if !self.holds(layout) {
                self.keep(layout);
            }
        }
        let mut out = sp_decoder::frame_pool::take(self.layout.len);
        let paint = Paint::Fade {
            from: self.side(from),
            to: self.side(to),
            weight,
        };
        mix_nv12_into(self.layout, paint, pool, &mut out);
        SharedFrame::new(out)
    }

    /// One fade side as the kernel reads it: the canvas black when missing,
    /// the picture as it is when the canvas holds it, else fitted by its
    /// kept plan ([`Canvas::fade`] keeps one for every side it fits).
    fn side<'a>(&'a self, side: FadeSide<'a>) -> Side<'a> {
        match side {
            None => Side::Black,
            Some((layout, picture)) if self.holds(layout) => Side::Same(picture),
            Some((layout, picture)) => {
                let canvas = self.layout;
                let plan = self
                    .plans
                    .iter()
                    .find(|plan| plan.fits(layout, canvas))
                    .expect("Canvas::fade keeps a plan for every side it fits");
                Side::Fitted(plan, picture)
            }
        }
    }
}

#[cfg(test)]
#[path = "program_canvas_tests.rs"]
mod tests;
