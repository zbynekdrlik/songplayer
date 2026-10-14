//! Aspect-kept placement of one picture inside another (pure, WASM-safe).
//!
//! Three paths place a picture the same way and differ only in how they scale
//! its pixels:
//!
//! - the #178 preview letterbox (`sp-server` `preview_stream::placement_for`):
//!   nearest-neighbour into its fixed 640×360 canvas;
//! - the #215 program fit (`sp-server` `program_transition::FitPlan`):
//!   bilinear into the 1920×1080 `SP-program` canvas, on the CPU;
//! - the #239 `SP-program` Spout sender (`sp-gpu`): bilinear into its fixed
//!   1920×1080 render target, on the GPU.
//!
//! One placement for all of them keeps a picture where the owner expects it
//! on every surface ("one app, one behaviour"). `sp-server` re-exports it as
//! `playback::nv12_fit`.
//!
//! #267: the #223 `SP-program-MAX` compositor (`sp-gpu`, a fixed 3840×2160
//! render target) is the LED wall's feed, and a wall must never show bars at
//! its sides, so MAX places a picture by [`fill_width`] instead: the picture
//! scaled to the canvas's whole width, the rows over its height cut off top
//! and bottom. For a picture as wide as the canvas or wider that is exactly
//! [`aspect_fit`].

/// Where a `sw×sh` picture goes inside a `dw×dh` one: its scaled size and its
/// centred offsets, all EVEN, so a 2×2-subsampled chroma sample stays on its
/// luma block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub w: u32,
    pub h: u32,
    pub off_x: u32,
    pub off_y: u32,
}

/// The largest aspect-kept placement of a `sw×sh` picture inside `dw×dh`.
/// Each axis is the full destination axis capped by the aspect-scaled other
/// axis (branch-free, so there is no `<=`-boundary comparison whose `<` mutant
/// would be equivalent on an exact aspect match; `min` picks the tighter
/// axis), floored to even, at least 2×2 (so under a destination smaller than
/// 2×2 it overhangs; the program fit caps it); the offsets centre it, floored
/// to even. A degenerate source is a zero-size image at the centre.
pub fn aspect_fit(sw: u32, sh: u32, dw: u32, dh: u32) -> Placement {
    if sw == 0 || sh == 0 {
        return Placement {
            w: 0,
            h: 0,
            off_x: (dw / 2) & !1,
            off_y: (dh / 2) & !1,
        };
    }
    let w = u64::from(dw).min(u64::from(sw) * u64::from(dh) / u64::from(sh)) as u32;
    let h = u64::from(dh).min(u64::from(sh) * u64::from(dw) / u64::from(sw)) as u32;
    let w = (w & !1).max(2);
    let h = (h & !1).max(2);
    Placement {
        w,
        h,
        off_x: (dw.saturating_sub(w) / 2) & !1,
        off_y: (dh.saturating_sub(h) / 2) & !1,
    }
}

/// #267: where a picture goes when it must fill the destination's WIDTH
/// ([`fill_width`]): the part of the destination it covers, and which rows of
/// the picture scaled to that width show there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// The part of the destination the picture covers: inside it, even, like
    /// an [`aspect_fit`] placement.
    pub place: Placement,
    /// The height of the picture scaled to `place.w`, at least `place.h`.
    pub scaled_h: u32,
    /// The rows cut off the top of the scaled picture (even): it shows rows
    /// `crop_top..crop_top + place.h`, and the rows under them are cut off
    /// its bottom.
    pub crop_top: u32,
}

impl Fill {
    /// A whole picture at `place`, nothing cut off: an [`aspect_fit`]
    /// placement as a [`Fill`].
    pub fn whole(place: Placement) -> Self {
        Self {
            place,
            scaled_h: place.h,
            crop_top: 0,
        }
    }
}

/// #267: a `sw×sh` picture scaled to the WHOLE width of `dw×dh`, aspect kept,
/// centred vertically. Its height `⌊sh·dw/sw⌋` (even, at least 2, `u32::MAX`
/// at most) is always computed the same way, with no branch on the aspect:
///
/// - narrower than the destination (4:3, 1:1, 9:16): taller than it, so it
///   covers the whole destination and the rows over its height are cut off,
///   as many at the top as at the bottom (the top count floored to even);
/// - as wide or wider (16:9, 21:9): no taller than it, so nothing is cut off
///   and the result is exactly [`aspect_fit`]'s letterbox ([`Fill::whole`]).
///
/// Like [`aspect_fit`]: sizes and offsets even (on an odd destination width
/// the last column stays black), at least 2×2 (under a destination smaller
/// than 2×2 it overhangs), and a degenerate source is [`aspect_fit`]'s zero
/// image at the even centre. The width is the destination's own, so the
/// picture always starts at column 0.
pub fn fill_width(sw: u32, sh: u32, dw: u32, dh: u32) -> Fill {
    if sw == 0 || sh == 0 {
        return Fill::whole(aspect_fit(sw, sh, dw, dh));
    }
    let w = (dw & !1).max(2);
    let scaled = u64::from(sh) * u64::from(dw) / u64::from(sw);
    let scaled_h = (u32::try_from(scaled).unwrap_or(u32::MAX) & !1).max(2);
    let h = (scaled_h.min(dh) & !1).max(2);
    Fill {
        place: Placement {
            w,
            h,
            off_x: 0,
            off_y: (dh.saturating_sub(h) / 2) & !1,
        },
        scaled_h,
        crop_top: ((scaled_h - h) / 2) & !1,
    }
}

#[cfg(test)]
#[path = "fit_tests.rs"]
mod tests;
