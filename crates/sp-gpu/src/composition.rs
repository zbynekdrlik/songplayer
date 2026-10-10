//! What one `SP-program-MAX` boundary shows, and the quads that draw it
//! (pure).
//!
//! The canvas is fixed: 3840×2160, the owner's rule (#223 revision 3: "4k",
//! "both outputs static"). #239: the same boundary also goes out as the
//! `SP-program` Spout sender, drawn into a fixed 1920×1080 target
//! ([`FHD_WIDTH`] × [`FHD_HEIGHT`], `SP-program`'s NDI canvas), so the
//! layers take their target's size ([`Composition::layers_in`]). A boundary
//! is the black (a genuine standby), one picture, or a fade of two. Each
//! picture is placed by `sp_core::fit::aspect_fit` into the target, the rule
//! of `SP-program`'s 1920×1080 canvas, and drawn as one quad over the black
//! target with
//! additive blending at its weight: the outgoing side at 1 − w first, then
//! the incoming one at w, with w the boundary's Q8 weight from `SP-program`
//! (`program_transition::weight_q8`). Where a side's quad does not reach
//! (its bars), it adds nothing, so the result is the CPU fade's
//! `from·(1 − w) + to·w` with each side's bars black.

use sp_core::fit::{Placement, aspect_fit};

use crate::picture::Nv12Picture;

/// The canvas width of `SP-program-MAX`.
pub const CANVAS_WIDTH: u32 = 3840;

/// The canvas height of `SP-program-MAX`.
pub const CANVAS_HEIGHT: u32 = 2160;

/// #239: the target width of the `SP-program` Spout sender: the FHD
/// program's fixed canvas (sp-server's `program_canvas.rs`, the picture the
/// NDI `SP-program` carries).
pub const FHD_WIDTH: u32 = 1920;

/// #239: the target height of the `SP-program` Spout sender.
pub const FHD_HEIGHT: u32 = 1080;

// A Q8 weight of 1, all of the incoming side: `SP-program`'s fade weight
// unit, the one `sp_core::blend` constant both outputs use.
pub use sp_core::blend::Q8_ONE;

/// What one boundary of `SP-program-MAX` shows.
#[derive(Debug, Clone, Copy)]
pub enum Composition<'a> {
    /// A genuine standby: the black canvas.
    Black,
    /// A plain boundary: one picture at full weight.
    Picture(Nv12Picture<'a>),
    /// A fade boundary: `from` at 1 − w, then `to` at w, w = `weight_q8` /
    /// 256 (capped at 256). A missing side is the black.
    Fade {
        from: Option<Nv12Picture<'a>>,
        to: Option<Nv12Picture<'a>>,
        weight_q8: u32,
    },
}

/// The texture slot a picture is uploaded into. A plain picture and a fade's
/// outgoing side use [`Slot::Outgoing`], a fade's incoming side
/// [`Slot::Incoming`]; a picture already resident in its slot is not
/// uploaded again (`residency::upload_for`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Outgoing,
    Incoming,
}

impl Slot {
    /// The slot's index in the compositor's two texture slots.
    pub fn index(self) -> usize {
        match self {
            Slot::Outgoing => 0,
            Slot::Incoming => 1,
        }
    }
}

/// One quad of a composition.
#[derive(Debug, Clone, Copy)]
pub struct Layer<'a> {
    pub slot: Slot,
    pub picture: Nv12Picture<'a>,
    /// Where the picture goes in the target.
    pub place: Placement,
    /// Its blend weight: the shader's RGB is multiplied by it and ADDED to
    /// the target.
    pub weight: f32,
}

impl<'a> Composition<'a> {
    /// The quads that draw it into `SP-program-MAX`'s 3840×2160 canvas:
    /// [`layers_in`](Self::layers_in) [`CANVAS_WIDTH`] × [`CANVAS_HEIGHT`].
    pub fn layers(&self) -> Vec<Layer<'a>> {
        self.layers_in(CANVAS_WIDTH, CANVAS_HEIGHT)
    }

    /// The quads to draw over a black `width`×`height` target, in draw
    /// order: the outgoing side, then the incoming one, each placed by
    /// `aspect_fit` into that target. A side of weight 0 is not drawn (and
    /// not uploaded): the black has no layer, a plain picture one at
    /// weight 1.
    pub fn layers_in(&self, width: u32, height: u32) -> Vec<Layer<'a>> {
        let (from, to, to_q8) = match *self {
            Composition::Black => (None, None, 0),
            Composition::Picture(picture) => (Some(picture), None, 0),
            Composition::Fade {
                from,
                to,
                weight_q8,
            } => (from, to, weight_q8.min(Q8_ONE)),
        };
        [
            (Slot::Outgoing, from, Q8_ONE - to_q8),
            (Slot::Incoming, to, to_q8),
        ]
        .into_iter()
        .filter(|&(_, _, q8)| q8 > 0)
        .filter_map(|(slot, picture, q8)| {
            picture.map(|picture| Layer {
                slot,
                picture,
                place: aspect_fit(picture.width, picture.height, width, height),
                weight: q8 as f32 / Q8_ONE as f32,
            })
        })
        .collect()
    }
}

#[cfg(test)]
#[path = "composition_tests.rs"]
mod tests;
