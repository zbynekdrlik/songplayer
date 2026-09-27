//! Aspect-kept placement of one NV12 picture inside another (pure).
//!
//! Two paths place a picture the same way and differ only in how they scale
//! its pixels:
//!
//! - the #178 preview letterbox (`preview_stream::letterbox_nv12_into`):
//!   nearest-neighbour into its fixed 640×360 canvas, on the decode thread, per
//!   watched frame — cheap on purpose (`preview.md` iron rule 2);
//! - the #215 program fit (`program_transition::FitPlan`): bilinear into the
//!   incoming picture's layout, on the `SP-program` sender, only on the mixed
//!   boundaries of a transition window.
//!
//! One placement for both keeps a picture where the owner expects it on every
//! surface ("one app, one behaviour").

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
/// axis), floored to even, at least 2×2; the offsets centre it, floored to
/// even. A degenerate source is a zero-size image at the centre.
pub fn aspect_fit(sw: u32, sh: u32, dw: u32, dh: u32) -> Placement {
    if sw == 0 || sh == 0 {
        return Placement {
            w: 0,
            h: 0,
            off_x: dw / 2,
            off_y: dh / 2,
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
