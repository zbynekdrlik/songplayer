//! What one `SP-program-MAX` boundary shows, and the quads that draw it
//! (pure).
//!
//! The canvas is fixed: 3840×2160, the owner's rule (#223 revision 3: "4k",
//! "both outputs static"). #239: the same boundary also goes out as the
//! `SP-program` Spout sender, drawn into a fixed 1920×1080 target
//! ([`FHD_WIDTH`] × [`FHD_HEIGHT`], `SP-program`'s NDI canvas), so the
//! layers take their target's size ([`Composition::layers_in`]). A boundary
//! is the black (a genuine standby), one picture, or a fade of two. Each
//! picture is placed into the target by the target's [`Fit`] and drawn as
//! one quad over the black target with additive blending at its weight: the
//! outgoing side at 1 − w first, then the incoming one at w, with w the
//! boundary's Q8 weight from `SP-program` (`program_transition::weight_q8`).
//! Where a side's quad does not reach (its bars), it adds nothing, so the
//! result is the CPU fade's `from·(1 − w) + to·w` with each side's bars
//! black.
//!
//! #251 step 2: both targets are drawn by ONE compositor from ONE upload of
//! each picture. A compositor holds several render targets ([`Target`]:
//! [`MAX_TARGET`], [`FHD_TARGET`]); [`Composition::uploads`] are what one
//! compose uploads (#279 P2: and [`Composition::frames`] the ring slots it
//! holds), and each target draws [`Composition::layers_on`] it:
//! the same pictures in the same slots at the same weights, placed by that
//! target's rule.
//!
//! #267: the two targets place a picture differently, each rule named once
//! here. MAX is the LED wall's feed: [`MAX_FIT`] fills the canvas's whole
//! width (`sp_core::fit::fill_width`; a 4:3 picture is cropped top and
//! bottom, never pillarboxed — Arena's Fill would carry the bars onto the
//! wall). The `SP-program` sender carries the FHD program the NDI
//! `SP-program` carries: [`FHD_FIT`] keeps `sp_core::fit::aspect_fit`, the
//! rule of `SP-program`'s 1920×1080 canvas. A cropped picture's quad covers
//! the part of the target inside it, and its texture coordinates
//! ([`Layer::uv_rect`]) show the part of the picture that lands there.
//!
//! #279 P2: a side is a [`Side`]: a picture in CPU memory, uploaded into
//! the side's texture [`Slot`] (what [`Composition::uploads`] lists), or a
//! decoded picture in a ring slot ([`GpuFrame`]), sampled in place (what
//! [`Composition::frames`] lists, each slot once). A ring slot's texture
//! has even sides, so its picture may be one column or row smaller: the
//! layer's texture coordinates are scaled into the slot
//! ([`uv_in_texture`]).

use sp_core::fit::{Fill, Placement, aspect_fit, fill_width};

use crate::picture::Nv12Picture;
use crate::quad::{uv_in_texture, uv_rect};
use crate::ring::GpuFrame;

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

/// How a target places each picture (#267).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// The whole picture, aspect kept, centred, black bars where it does not
    /// reach (`sp_core::fit::aspect_fit`): `SP-program`'s rule.
    Aspect,
    /// The target's whole width, aspect kept, centred, the rows over the
    /// target's height cut off top and bottom (`sp_core::fit::fill_width`);
    /// a picture as wide as the target or wider is letterboxed exactly as by
    /// [`Fit::Aspect`].
    FillWidth,
}

impl Fit {
    /// Where this rule puts a `width`×`height` picture in a
    /// `target_width`×`target_height` target.
    pub fn place(self, width: u32, height: u32, target_width: u32, target_height: u32) -> Fill {
        match self {
            Fit::Aspect => Fill::whole(aspect_fit(width, height, target_width, target_height)),
            Fit::FillWidth => fill_width(width, height, target_width, target_height),
        }
    }
}

/// #267: `SP-program-MAX`'s rule. MAX is the LED wall's feed (Arena scales
/// it to the composition's width), so a picture fills the canvas's whole
/// width and never carries bars at its sides.
pub const MAX_FIT: Fit = Fit::FillWidth;

/// #267: the `SP-program` Spout sender's rule (#239): `SP-program`'s own,
/// the whole picture with its bars, as the NDI `SP-program` carries it.
pub const FHD_FIT: Fit = Fit::Aspect;

/// #251 step 2: a render target a compositor draws: its size and the rule
/// that places each picture in it, fixed for the target's life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub width: u32,
    pub height: u32,
    pub fit: Fit,
}

/// `SP-program-MAX`'s target: the fixed 3840×2160 canvas, each picture
/// filling its width ([`MAX_FIT`]).
pub const MAX_TARGET: Target = Target {
    width: CANVAS_WIDTH,
    height: CANVAS_HEIGHT,
    fit: MAX_FIT,
};

/// The `SP-program` Spout sender's target (#239): the FHD program's
/// 1920×1080, each picture with its bars ([`FHD_FIT`]).
pub const FHD_TARGET: Target = Target {
    width: FHD_WIDTH,
    height: FHD_HEIGHT,
    fit: FHD_FIT,
};

/// #279 P2: one picture of a boundary: in CPU memory, uploaded into its
/// texture slot, or in a ring slot on the GPU, sampled in place.
#[derive(Debug, Clone, Copy)]
pub enum Side<'a> {
    /// A picture in CPU memory: uploaded (unless resident, `upload_for`).
    Cpu(Nv12Picture<'a>),
    /// A decoded picture in a ring slot: opened once, drawn under its
    /// keyed mutex, never uploaded.
    Gpu(&'a GpuFrame),
}

impl<'a> Side<'a> {
    /// The picture's id: an uploaded picture's (its caller's label), a ring
    /// frame's own (process-wide). The two kinds number apart and may share
    /// a value: an id names a picture only among its kind (the residency
    /// compares the uploaded pictures' alone, the ring slots go by
    /// [`SlotKey`](crate::SlotKey)).
    pub fn id(&self) -> u64 {
        match self {
            Side::Cpu(picture) => picture.id,
            Side::Gpu(frame) => frame.id(),
        }
    }

    /// The picture's width.
    pub fn width(&self) -> u32 {
        match self {
            Side::Cpu(picture) => picture.width,
            Side::Gpu(frame) => frame.width(),
        }
    }

    /// The picture's height.
    pub fn height(&self) -> u32 {
        match self {
            Side::Cpu(picture) => picture.height,
            Side::Gpu(frame) => frame.height(),
        }
    }

    /// The size of the texture the side is sampled from: an uploaded
    /// picture's own, a ring slot's (its picture's, rounded up to even).
    pub fn texture_size(&self) -> (u32, u32) {
        match self {
            Side::Cpu(picture) => (picture.width, picture.height),
            Side::Gpu(frame) => frame.texture_size(),
        }
    }

    /// The picture to upload; `None` for a ring frame.
    pub fn cpu(&self) -> Option<Nv12Picture<'a>> {
        match *self {
            Side::Cpu(picture) => Some(picture),
            Side::Gpu(_) => None,
        }
    }

    /// The ring frame; `None` for a picture to upload.
    pub fn gpu(&self) -> Option<&'a GpuFrame> {
        match *self {
            Side::Cpu(_) => None,
            Side::Gpu(frame) => Some(frame),
        }
    }
}

impl<'a> From<Nv12Picture<'a>> for Side<'a> {
    fn from(picture: Nv12Picture<'a>) -> Self {
        Side::Cpu(picture)
    }
}

impl<'a> From<&'a GpuFrame> for Side<'a> {
    fn from(frame: &'a GpuFrame) -> Self {
        Side::Gpu(frame)
    }
}

/// What one boundary of `SP-program-MAX` shows.
#[derive(Debug, Clone, Copy)]
pub enum Composition<'a> {
    /// A genuine standby: the black canvas.
    Black,
    /// A plain boundary: one picture at full weight.
    Picture(Side<'a>),
    /// A fade boundary: `from` at 1 − w, then `to` at w, w = `weight_q8` /
    /// 256 (capped at 256). A missing side is the black.
    Fade {
        from: Option<Side<'a>>,
        to: Option<Side<'a>>,
        weight_q8: u32,
    },
}

/// The texture slot a picture is uploaded into. A plain picture and a fade's
/// outgoing side use [`Slot::Outgoing`], a fade's incoming side
/// [`Slot::Incoming`]; a picture already resident in its slot is not
/// uploaded again (`residency::upload_for`), nor one resident in the other
/// slot: the slots trade places first (#251, `residency::swap_slots`).
/// #279 P2: a ring frame's side keeps its slot (its draw order) but holds
/// no texture there.
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
    /// #279 P2: the picture it draws: uploaded, or in a ring slot.
    pub side: Side<'a>,
    /// The part of the target the quad covers: inside the target, the
    /// picture's whole placement unless its [`Fit`] cut rows off.
    pub place: Placement,
    /// #267: the part of the picture the quad shows, in texture coordinates
    /// (left, top, right, bottom; `[0, 0, 1, 1]` = the whole picture):
    /// `quad::uv_rect` of its [`Fill`], #279 P2 scaled into a ring slot's
    /// larger texture (`quad::uv_in_texture`; unchanged for an uploaded
    /// picture). The shader and the CPU reference both read it.
    pub uv_rect: [f32; 4],
    /// Its blend weight: the shader's RGB is multiplied by it and ADDED to
    /// the target.
    pub weight: f32,
}

impl<'a> Composition<'a> {
    /// The quads that draw it into `SP-program-MAX`'s 3840×2160 canvas:
    /// [`layers_on`](Self::layers_on) [`MAX_TARGET`].
    pub fn layers(&self) -> Vec<Layer<'a>> {
        self.layers_on(MAX_TARGET)
    }

    /// #251 step 2: the quads that draw it into `target`:
    /// [`layers_in`](Self::layers_in) its size by its rule.
    pub fn layers_on(&self, target: Target) -> Vec<Layer<'a>> {
        self.layers_in(target.width, target.height, target.fit)
    }

    /// The quads to draw over a black `width`×`height` target, in draw
    /// order: the outgoing side, then the incoming one, each placed into
    /// that target by `fit`. A side of weight 0 is not drawn (and not
    /// uploaded): the black has no layer, a plain picture one at weight 1.
    /// Whatever the target, the layers carry the same slots and sides
    /// (`pictures`, the tests' view).
    pub fn layers_in(&self, width: u32, height: u32, fit: Fit) -> Vec<Layer<'a>> {
        self.sides()
            .map(|(slot, side, q8)| {
                let picture = (side.width(), side.height());
                let fill = fit.place(picture.0, picture.1, width, height);
                Layer {
                    slot,
                    side,
                    place: fill.place,
                    uv_rect: uv_in_texture(uv_rect(fill), picture, side.texture_size()),
                    weight: q8 as f32 / Q8_ONE as f32,
                }
            })
            .collect()
    }

    /// #251 step 2: the sides it draws, each in its slot, in draw order,
    /// whatever targets it draws (each target's
    /// [`layers_on`](Self::layers_on) samples exactly these). #279 P2: one
    /// compose uploads their CPU pictures ([`uploads`](Self::uploads)) and
    /// opens their ring frames ([`frames`](Self::frames)); this list is the
    /// tests' view of both.
    #[cfg(test)]
    pub(crate) fn pictures(&self) -> Vec<(Slot, Side<'a>)> {
        self.sides().map(|(slot, side, _)| (slot, side)).collect()
    }

    /// #279 P2: the CPU pictures it draws, each in its texture slot, in
    /// draw order: what ONE compose uploads.
    pub fn uploads(&self) -> Vec<(Slot, Nv12Picture<'a>)> {
        self.sides()
            .filter_map(|(slot, side, _)| side.cpu().map(|picture| (slot, picture)))
            .collect()
    }

    /// #279 P2: the ring frames it draws, in draw order, each ring slot
    /// ONCE (both sides of a fade may show the same slot): what one compose
    /// opens and acquires.
    pub fn frames(&self) -> Vec<&'a GpuFrame> {
        let mut frames: Vec<&'a GpuFrame> = Vec::new();
        for frame in self.sides().filter_map(|(_, side, _)| side.gpu()) {
            if frames.iter().all(|held| held.key() != frame.key()) {
                frames.push(frame);
            }
        }
        frames
    }

    /// The drawn sides: slot, side and Q8 weight, in draw order. A side of
    /// weight 0, or with no picture, is not drawn.
    fn sides(&self) -> impl Iterator<Item = (Slot, Side<'a>, u32)> {
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
        .filter_map(|(slot, side, q8)| side.map(|side| (slot, side, q8)))
    }
}

#[cfg(test)]
#[path = "composition_tests.rs"]
mod tests;
