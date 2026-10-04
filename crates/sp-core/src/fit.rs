//! Aspect-kept placement of one picture inside another (pure, WASM-safe).
//!
//! Three paths place a picture the same way and differ only in how they scale
//! its pixels:
//!
//! - the #178 preview letterbox (`sp-server` `preview_stream::placement_for`):
//!   nearest-neighbour into its fixed 640×360 canvas;
//! - the #215 program fit (`sp-server` `program_transition::FitPlan`):
//!   bilinear into the 1920×1080 `SP-program` canvas, on the CPU;
//! - the #223 `SP-program-MAX` compositor (`sp-gpu`): bilinear into its fixed
//!   3840×2160 render target, on the GPU.
//!
//! One placement for all of them keeps a picture where the owner expects it
//! on every surface ("one app, one behaviour"). `sp-server` re-exports it as
//! `playback::nv12_fit`.

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

#[cfg(test)]
#[path = "fit_tests.rs"]
mod tests;
