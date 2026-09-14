//! Mutation-killing unit tests for `burn_overlay` (#151) — the pure NV12 QR
//! paint math. Each test pins an EXACT observed value at a precise input so a
//! single operator / boundary / index mutation flips it. `super::*` resolves to
//! the `burn_overlay` module under test (`render_qr_modules`, `paint_nv12`,
//! `BurnGeom`, the luma/chroma constants).
//!
//! Deterministic fixtures only: a 2-byte payload ("hi") is a QR **version 1**
//! (21×21 modules, the smallest normal version — qrcode 0.14.1 `encode_auto`
//! never emits Micro QR), so `side = 21 + 2*QUIET_ZONE(4) = 29`; and the QR
//! finder-pattern module colours at content (0,0)/(0,1)/(1,1) are fixed by the
//! QR spec independent of the payload.

use super::*;

/// A `module_side × module_side` checkerboard (dark iff `(my+mx)` is even),
/// matching the existing test's convention: `modules[my*module_side + mx]`.
fn checkerboard(module_side: usize) -> Vec<bool> {
    (0..module_side * module_side)
        .map(|i| ((i / module_side) + (i % module_side)) % 2 == 0)
        .collect()
}

// ---------------------------------------------------------------------------
// render_qr_modules — side arithmetic + module placement
// ---------------------------------------------------------------------------

/// `side = code.width() + 2*QUIET_ZONE`. For a version-1 QR that is
/// `21 + 8 = 29`, and the grid is `29*29 = 841`.
///
/// Kills 32:18 `+ -> *` (`w*2*QZ = 21*8 = 168`) and 32:22 `* -> +`
/// (`w + 2 + QZ = 27`) — both diverge from 29 (and 841).
#[test]
fn render_qr_side_is_width_plus_two_quiet_zones() {
    let (side, modules) = render_qr_modules("hi").unwrap();
    assert_eq!(side, 29, "version-1 QR width (21) + 2 * QUIET_ZONE (4)");
    assert_eq!(modules.len(), 841, "grid is side * side");
}

/// The top-left finder pattern's outer corner (content (0,0)) is ALWAYS dark and
/// is placed at grid `(QUIET_ZONE, QUIET_ZONE)` = `modules[4*side + 4]`.
///
/// Kills 36:34 `== -> !=` (content (0,0) IS dark → the inverted test never sets
/// it → false) and 37:49 `+ -> -` (content (0,0) is written to `4*side - 4`, and
/// no source cell maps to `4*side + 4` under the flipped index, so it stays
/// false).
#[test]
fn render_qr_finder_corner_module_is_dark() {
    let (side, modules) = render_qr_modules("hi").unwrap();
    assert!(
        modules[4 * side + 4],
        "finder-pattern corner (content 0,0) must be a dark module"
    );
}

/// Each source colour must reach its OWN destination via `colors[y*w + x]`.
/// Content (0,1) is the finder's dark top border; content (1,1) is a light
/// inner-ring module.
///
/// Kills 36:25 `* -> +` (output (0,1) would read `colors[0+w+1]` = content
/// (1,1) = light, leaving `modules[4*side+5]` false) and 36:25 `* -> /` (output
/// (1,1) would read `colors[1/w+1]` = content (0,1) = dark, wrongly setting
/// `modules[5*side+5]` true).
#[test]
fn render_qr_maps_each_source_cell_to_its_own_module() {
    let (side, modules) = render_qr_modules("hi").unwrap();
    // Output (0,1) -> grid (4, 5): finder top border, dark.
    assert!(
        modules[4 * side + 5],
        "content (0,1) (finder top border) must be dark"
    );
    // Output (1,1) -> grid (5, 5): finder inner ring, light.
    assert!(
        !modules[5 * side + 5],
        "content (1,1) (finder inner ring) must be light"
    );
}

// ---------------------------------------------------------------------------
// paint_nv12 — interior luma mapping (side=12 → scale=2, rendered=10, pad=1)
// ---------------------------------------------------------------------------

/// Paint a 5×5 checkerboard into a 16×16 NV12 with `side = 12` so `scale = 2`,
/// `rendered = 10`, `pad = (12-10)/2 = 1` (a NON-zero pad, unlike side==rendered).
/// Every asserted pixel pins the exact luma the correct mapping produces.
///
/// Kills:
/// - 82:26 `* -> +` (`rendered = 2+5 = 7` → `pad = 2` → pixel (5,5) becomes border light)
/// - 83:21 `- -> /` and 83:33 `/ -> %` (`pad = 0` → pixel (4,4) becomes interior dark)
/// - 83:33 `/ -> *` (`pad = 4` → pixel (5,5) becomes border light)
/// - 99:29 `< -> <=` and 99:65 `< -> <=` (at `ry==pad`/`rx==pad`, pixel (5,5) → light)
/// - 102:37 `/ -> %` (`mx` mis-scaled → pixel (10,5) → light)
/// - 103:37 `/ -> %` (`my` mis-scaled → pixel (5,10) → light)
/// - 104:31 `* -> +` and `* -> /` (wrong module index → pixel (5,7) → dark)
#[test]
fn paint_nv12_interior_luma_is_exact_with_nonzero_pad() {
    let (w, h, stride) = (16u32, 16u32, 16u32);
    let y_len = (stride * h) as usize;
    let uv_len = (stride * (h / 2)) as usize;
    let sentinel = 100u8;
    let mut buf = vec![sentinel; y_len + uv_len];

    let geom = BurnGeom {
        side: 12,
        margin: 2,
        x: 4,
        y: 4,
    };
    let module_side = 5usize;
    let modules = checkerboard(module_side);

    paint_nv12(&mut buf, w, h, stride, geom, module_side, &modules);

    let stride = stride as usize;
    let px = |x: usize, y: usize| buf[y * stride + x];

    // (4,4): rx0/ry0 → rx<pad(1) → border light.
    assert_eq!(px(4, 4), 235, "(4,4) is the left/top quiet border");
    // (5,5): rx1/ry1 == pad → interior module(my0,mx0) → dark.
    assert_eq!(px(5, 5), 16, "(5,5) is interior module (0,0), dark");
    // (10,5): rx6/ry1 → mx=(6-1)/2=2, my=0 → module(0,2) dark.
    assert_eq!(px(10, 5), 16, "(10,5) is interior module (0,2), dark");
    // (5,10): rx1/ry6 → mx=0, my=(6-1)/2=2 → module(2,0) dark.
    assert_eq!(px(5, 10), 16, "(5,10) is interior module (2,0), dark");
    // (5,7): rx1/ry3 → mx=0, my=(3-1)/2=1 → module(1,0) light.
    assert_eq!(px(5, 7), 235, "(5,7) is interior module (1,0), light");
    // A pixel fully outside the rectangle is never touched.
    assert_eq!(px(3, 3), sentinel, "(3,3) is outside the burn rectangle");
}

// ---------------------------------------------------------------------------
// paint_nv12 — early-return guards
// ---------------------------------------------------------------------------

/// `modules.len() < module_side*module_side` is a no-op guard. With
/// `module_side=5` (needs 25) but only 12 modules, the correct code returns the
/// buffer untouched.
///
/// Kills 64:42 `< -> >` (`12 > 25` false → proceeds), 64:56 `* -> +`
/// (`12 < 10` false → proceeds) and 64:56 `* -> /` (`12 < 1` false → proceeds):
/// each proceeds into the interior and indexes `modules[..24]` out of bounds
/// (len 12) → panics, while the correct code leaves `buf` unchanged.
#[test]
fn paint_nv12_returns_early_on_too_few_modules() {
    let mut buf = vec![7u8; 16 * 16 * 3 / 2];
    let before = buf.clone();
    let geom = BurnGeom {
        side: 10,
        margin: 2,
        x: 4,
        y: 4,
    };
    paint_nv12(&mut buf, 16, 16, 16, geom, 5, &[false; 12]);
    assert_eq!(buf, before, "too few modules must be a no-op");
}

/// `module_side == 0` is a no-op guard. Correct code returns before the
/// `side / module_side` division.
///
/// Kills 64:25 `|| -> &&`: `0==0 && 25 < 0` is false → the code proceeds and
/// divides by zero at `side / module_side`, panicking, whereas the correct
/// code leaves `buf` unchanged.
#[test]
fn paint_nv12_returns_early_on_zero_module_side() {
    let mut buf = vec![7u8; 16 * 16 * 3 / 2];
    let before = buf.clone();
    let geom = BurnGeom {
        side: 10,
        margin: 2,
        x: 4,
        y: 4,
    };
    paint_nv12(&mut buf, 16, 16, 16, geom, 0, &[false; 25]);
    assert_eq!(buf, before, "zero module_side must be a no-op");
}

/// A buffer EXACTLY the size of the luma plane (`nv12.len() == y_len`, no chroma
/// plane) is still large enough to paint: the guard is `nv12.len() < y_len`.
///
/// Kills 75:19 `< -> <=` (`256 <= 256` → wrongly returns) and 75:19 `< -> ==`
/// (`256 == 256` → wrongly returns): both leave the anchor pixel at its
/// sentinel instead of the painted dark value.
#[test]
fn paint_nv12_paints_when_buffer_is_exactly_the_luma_plane() {
    let (w, h, stride) = (16u32, 16u32, 16u32);
    let y_len = (stride * h) as usize; // 256, no chroma plane
    let mut buf = vec![100u8; y_len];

    let geom = BurnGeom {
        side: 10,
        margin: 2,
        x: 4,
        y: 4,
    };
    let module_side = 5usize;
    let modules = checkerboard(module_side);

    paint_nv12(&mut buf, w, h, stride, geom, module_side, &modules);

    // (4,4) = rx0/ry0, pad0 → module(0,0), dark. Sentinel is 100, so the
    // early-return mutants (which never paint) leave 100 here.
    assert_eq!(
        buf[4 * stride as usize + 4],
        16,
        "luma plane must be painted"
    );
}

// ---------------------------------------------------------------------------
// paint_nv12 — chroma plane
// ---------------------------------------------------------------------------

/// `chroma_rows = uv_plane.len() / stride` bounds the chroma loop. Craft a short
/// uv plane (52 bytes, stride 16 → 3 rows) with a rect that would span 4 chroma
/// rows so row 3 is clamped away.
///
/// Kills 121:38 `/ -> *`: `52 * 16` is huge, so the `.min(chroma_rows)` clamp no
/// longer bites and chroma row 3 (`uv_plane[48]`) gets painted to 128 instead of
/// staying at the 50 sentinel.
#[test]
fn paint_nv12_chroma_row_count_uses_division() {
    let stride = 16usize;
    let y_len = stride * 2; // 32
    let uv_len = 52usize; // 3 full rows + 4 bytes
    let mut buf = vec![50u8; y_len + uv_len];

    let geom = BurnGeom {
        side: 8,
        margin: 0,
        x: 0,
        y: 0,
    };
    let module_side = 4usize;
    let modules = checkerboard(module_side);

    paint_nv12(&mut buf, 16, 2, stride as u32, geom, module_side, &modules);

    // uv_plane starts at y_len. Row 0 col 0 is painted; row 3 is clamped away.
    assert_eq!(buf[y_len], 128, "chroma row 0 is painted neutral");
    assert_eq!(
        buf[y_len + 48],
        50,
        "chroma row 3 is beyond chroma_rows and must stay untouched"
    );
}

/// The `u + 1 >= uv_plane.len()` guard protects the *pair* write `uv[u]; uv[u+1]`.
/// Craft a uv plane of 33 bytes and a wide rect so the chroma walk reaches
/// `u = 32` (== len - 1) inside an allowed row.
///
/// Kills 126:18 `+ -> *` (`u * 1 >= len` → `32 >= 33` false → writes `uv[33]`,
/// out of bounds) and 126:18 `+ -> -` (`u - 1 >= len` → `31 >= 33` false →
/// writes `uv[33]`): both panic, while the correct guard breaks and leaves
/// `uv_plane[32]` at its sentinel.
#[test]
fn paint_nv12_chroma_pair_guard_needs_room_for_both_bytes() {
    let stride = 16usize;
    let y_len = stride * 2; // 32
    let uv_len = 33usize; // 2 full rows + 1 byte
    let mut buf = vec![50u8; y_len + uv_len];

    let geom = BurnGeom {
        side: 18,
        margin: 0,
        x: 0,
        y: 0,
    };
    let module_side = 5usize;
    let modules = checkerboard(module_side);

    paint_nv12(&mut buf, 16, 2, stride as u32, geom, module_side, &modules);

    // Row 1 up to cx=7 is painted (u=30 -> uv[30],uv[31]); cx=8 (u=32) hits the
    // guard and breaks, so uv_plane[32] stays at the sentinel.
    assert_eq!(buf[y_len + 30], 128, "chroma row 1 col 7 is painted");
    assert_eq!(
        buf[y_len + 32],
        50,
        "the final odd byte has no room for a pair and must stay untouched"
    );
}
