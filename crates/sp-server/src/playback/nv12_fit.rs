//! Aspect-kept placement of one NV12 picture inside another (pure).
//!
//! Two paths here place a picture the same way and differ only in how they
//! scale its pixels:
//!
//! - the #178 preview letterbox (`preview_stream::letterbox_nv12_into`):
//!   nearest-neighbour into its fixed 640×360 canvas, on the decode thread, per
//!   watched frame — cheap on purpose (`preview.md` iron rule 2);
//! - the #215 program fit (`program_transition::FitPlan`): bilinear into the
//!   `SP-program` canvas (#223: 1920×1080, `program_canvas.rs`), on the
//!   `SP-program` sender, for every picture that is not already a canvas
//!   picture.
//!
//! #223 S1a: the rule itself lives in `sp_core::fit`, so the `SP-program-MAX`
//! GPU compositor (`sp-gpu`) places a picture by the same arithmetic. This
//! module re-exports it under its old path. One placement for every surface
//! keeps a picture where the owner expects it ("one app, one behaviour").

pub use sp_core::fit::{Placement, aspect_fit};
