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
//! - any other picture is fitted onto the canvas's studio black. It is placed
//!   by `nv12_fit::aspect_fit` (aspect kept, centred on even offsets, bars
//!   Y 16 / UV 128) and scaled bilinear by the #215 fused kernel
//!   (`mix_nv12_into` with `Outgoing::Fitted`) at weight 0: every byte is the
//!   fitted picture's, a plain fit with no blend. It runs in the sender's row
//!   bands into a `frame_pool` buffer. A larger picture is scaled down, a
//!   smaller one up, and a 16:9 one fills the canvas.
//!
//! A fit's plan (its column taps) is built once per source layout. The canvas
//! keeps the [`FIT_PLANS_KEPT`] plans used last, so a fade between two sizes,
//! which fits both sides on every boundary, rebuilds nothing; each new plan
//! is one INFO line.

use tracing::info;

use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_transition::{FitPlan, Layout, Outgoing, mix_nv12_into};

/// How many fit plans the canvas keeps: both sides of a fade.
pub const FIT_PLANS_KEPT: usize = 2;

/// The Q8 weight of the black canvas when a picture is only fitted onto it:
/// none, so every byte is the fitted picture's (`(f·256 + 128) >> 8 = f`).
const FIT_ONLY_Q8: u32 = 0;

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

    /// The plan that fits a `src` picture into the canvas. A kept one becomes
    /// the one used last; otherwise a new one is built (logged), and the plan
    /// used longest ago is dropped past [`FIT_PLANS_KEPT`].
    pub fn plan(&mut self, src: Layout) -> &FitPlan {
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
        &self.plans[0]
    }

    /// `video`, a `layout` picture, as a canvas picture: `video` itself (the
    /// same allocation) when the canvas holds it, else fitted onto `black`
    /// (the canvas's studio black) in `bands` row bands, into a pooled buffer
    /// of exactly the canvas's bytes. `black` is read for its length only (at
    /// weight 0 none of its bytes shows); one shorter than the canvas panics
    /// here, never a short picture labelled as the canvas for the SDK.
    pub fn fit(
        &mut self,
        layout: Layout,
        video: &SharedFrame,
        black: &[u8],
        bands: usize,
    ) -> SharedFrame {
        if self.holds(layout) {
            return video.clone();
        }
        let len = self.layout.len;
        let black = &black[..len];
        let mut out = sp_decoder::frame_pool::take(len);
        let plan = self.plan(layout);
        mix_nv12_into(
            Outgoing::Fitted(plan, video),
            black,
            FIT_ONLY_Q8,
            bands,
            &mut out,
        );
        SharedFrame::new(out)
    }
}

#[cfg(test)]
#[path = "program_canvas_tests.rs"]
mod tests;
