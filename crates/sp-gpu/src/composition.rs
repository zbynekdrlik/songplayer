//! What one `SP-program-MAX` boundary shows, and the quads that draw it
//! (pure).
//!
//! The canvas is fixed: 3840×2160, the owner's rule (#223 revision 3: "4k",
//! "both outputs static"). A boundary is the black (a genuine standby), one
//! picture, or a fade of two. Each picture is placed by
//! `sp_core::fit::aspect_fit` into the canvas, the rule of `SP-program`'s
//! 1920×1080 canvas, and drawn as one quad over the black canvas with
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

/// A Q8 weight of 1: all of the incoming side (`SP-program`'s fade weight
/// unit).
pub const Q8_ONE: u32 = 256;

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
    /// Where the picture goes in the canvas.
    pub place: Placement,
    /// Its blend weight: the shader's RGB is multiplied by it and ADDED to
    /// the canvas.
    pub weight: f32,
}

impl<'a> Composition<'a> {
    /// The quads to draw over the black canvas, in draw order: the outgoing
    /// side, then the incoming one. A side of weight 0 is not drawn (and not
    /// uploaded): the black has no layer, a plain picture one at weight 1.
    pub fn layers(&self) -> Vec<Layer<'a>> {
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
                place: aspect_fit(picture.width, picture.height, CANVAS_WIDTH, CANVAS_HEIGHT),
                weight: q8 as f32 / Q8_ONE as f32,
            })
        })
        .collect()
    }
}

#[cfg(test)]
#[path = "composition_tests.rs"]
mod tests;
