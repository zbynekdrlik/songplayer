//! #279 P1b: the burn on a UYVY picture ([`paint_uyvy`], [`burned_uyvy`]),
//! what `SP-program` sends from the GPU readback: the canvas burn's square
//! and modules in the pair layout (`U Y0 V Y1`), the same luma as the NV12
//! burn, and a round trip through rqrr reading the luma bytes (every odd
//! byte). Wired via `#[cfg(test)] #[path = "program_burn_tests_uyvy.rs"]
//! mod tests_uyvy;` in `program_burn.rs`.

use super::tests::checkerboard;
use super::{
    BURN_MODULES, SONGPLAYER_RUN_ID, burned, burned_uyvy, geometry, paint_uyvy, payload, qr_modules,
};
use crate::playback::program_transition::Layout;

const W: u32 = 1920;
const H: u32 = 1080;

/// A burn's wire stamp, in ns.
const GEN_TS_NS: i64 = 1_760_000_000_000_000_000;

/// A `w`×`h` UYVY picture of luma `y`, chroma `uv`.
fn picture(w: u32, h: u32, y: u8, uv: u8) -> Vec<u8> {
    [uv, y, uv, y].repeat(w as usize / 2 * h as usize)
}

/// What `paint_uyvy` must leave, written out pixel by pixel: in the
/// square's rows, a pixel inside the square takes its module's luma, and
/// a pixel whose PAIR overlaps the square a neutral chroma byte.
fn expected(before: &[u8], w: u32, h: u32, modules: &[bool]) -> Vec<u8> {
    let g = geometry(w, h).unwrap();
    let (n, m) = (BURN_MODULES as usize, g.module as usize);
    let (x0, y0, side) = (g.x as usize, g.y as usize, g.side as usize);
    let mut out = before.to_vec();
    for y in y0..y0 + side {
        for x in 0..w as usize {
            let at = (y * w as usize + x) * 2;
            let pair = x - x % 2;
            if pair + 1 >= x0 && pair < x0 + side {
                out[at] = 128;
            }
            if (x0..x0 + side).contains(&x) {
                let dark = modules[(y - y0) / m * n + (x - x0) / m];
                out[at + 1] = if dark { 16 } else { 235 };
            }
        }
    }
    out
}

/// The byte of pixel (`x`, `y`) of a `w`-wide UYVY picture: its chroma
/// (`luma = false`; U for an even pixel, V for an odd one) or its luma.
fn byte(buf: &[u8], w: u32, x: usize, y: usize, luma: bool) -> u8 {
    buf[(y * w as usize + x) * 2 + usize::from(luma)]
}

#[test]
fn a_uyvy_burn_sets_the_square_by_module_and_touches_nothing_else() {
    let before = picture(W, H, 100, 50);
    let modules = checkerboard();
    let mut buf = before.clone();
    paint_uyvy(&mut buf, W, H, &modules);
    assert!(buf == expected(&before, W, H, &modules));
    // Spot checks: the square is (1760, 12, 148, 148), 4 px a module.
    let at = |x, y, luma| byte(&buf, W, x, y, luma);
    assert_eq!(at(1760, 12, true), 16, "module (0, 0) is dark");
    assert_eq!(at(1764, 12, true), 235, "module (0, 1) is light");
    assert_eq!(at(1760, 16, true), 235, "module (1, 0) is light");
    assert_eq!(at(1907, 159, true), 16, "module (36, 36) is dark");
    assert_eq!((at(1760, 12, false), at(1761, 12, false)), (128, 128));
    assert_eq!((at(1906, 159, false), at(1907, 159, false)), (128, 128));
    assert_eq!((at(1759, 12, true), at(1759, 12, false)), (100, 50), "left");
    assert_eq!(
        (at(1908, 12, true), at(1908, 12, false)),
        (100, 50),
        "right"
    );
    assert_eq!(
        (at(1760, 11, true), at(1760, 11, false)),
        (100, 50),
        "above"
    );
    assert_eq!(
        (at(1760, 160, true), at(1760, 160, false)),
        (100, 50),
        "below"
    );
}

/// A 1 px module (270 rows): the square ends on an odd pixel, so the
/// pair it ends in takes the neutral chroma (both of its bytes), and its
/// other pixel keeps its luma.
#[test]
fn a_uyvy_burn_neutralises_the_whole_pair_its_odd_edge_ends_in() {
    let (w, h) = (64, 270);
    let g = geometry(w, h).unwrap();
    assert_eq!((g.module, g.side, g.x, g.y), (1, 37, 24, 3));
    let before = picture(w, h, 100, 50);
    let modules = checkerboard();
    let mut buf = before.clone();
    paint_uyvy(&mut buf, w, h, &modules);
    assert!(buf == expected(&before, w, h, &modules));
    let at = |x, y, luma| byte(&buf, w, x, y, luma);
    assert_eq!(at(60, 3, true), 16, "module (0, 36) is dark");
    assert_eq!((at(60, 3, false), at(61, 3, false)), (128, 128));
    assert_eq!(at(61, 3, true), 100, "outside the square");
    assert_eq!((at(62, 3, false), at(62, 3, true)), (50, 100), "next pair");
    assert_eq!(at(25, 4, true), 16, "module (1, 1) is dark");
    assert_eq!(at(24, 39, true), 16, "module (36, 0) is dark");
    assert_eq!(at(24, 40, true), 100, "below the square");
}

#[test]
fn a_uyvy_burn_paints_nothing_it_cannot_place() {
    let modules = checkerboard();
    let before = picture(W, H, 100, 50);
    let mut buf = before.clone();
    paint_uyvy(&mut buf, W, H, &modules[1..]);
    assert!(buf == before, "a grid that is not 37 × 37");
    // One byte short of whole UYVY.
    let mut short = before[..before.len() - 1].to_vec();
    paint_uyvy(&mut short, W, H, &modules);
    assert!(short[..] == before[..before.len() - 1], "not whole UYVY");
    let small = picture(W, 200, 100, 50);
    let mut buf = small.clone();
    paint_uyvy(&mut buf, W, 200, &modules);
    assert!(buf == small, "under 270 rows");
    // Exactly whole: painted.
    let mut whole = before.clone();
    assert_eq!(whole.len(), (W * 2 * H) as usize);
    paint_uyvy(&mut whole, W, H, &modules);
    assert_ne!(whole, before);
}

/// The NV12 canvas picture `w`×`h` of luma `y` (chroma 128).
fn nv12(w: u32, h: u32, y: u8) -> (Vec<u8>, Layout) {
    let luma = (w * h) as usize;
    let mut data = vec![y; luma];
    data.resize(luma + luma / 2, 128);
    let layout = Layout {
        width: w,
        height: h,
        stride: w,
        len: data.len(),
    };
    (data, layout)
}

/// Every QR rqrr reads in the luma of the `w`-wide UYVY picture `uyvy`,
/// inside `[x, x + rw) × [y, y + rh)`.
fn decode(uyvy: &[u8], w: u32, (x0, y0, rw, rh): (usize, usize, usize, usize)) -> Vec<String> {
    let stride = w as usize * 2;
    let mut image = rqrr::PreparedImage::prepare_from_greyscale(rw, rh, |x, y| {
        uyvy[(y0 + y) * stride + 2 * (x0 + x) + 1]
    });
    image
        .detect_grids()
        .into_iter()
        .filter_map(|grid| grid.decode().ok().map(|(_, text)| text))
        .collect()
}

#[test]
fn burned_uyvy_paints_a_copy_that_decodes_to_its_payload() {
    let text = payload(SONGPLAYER_RUN_ID, 1800, GEN_TS_NS);
    let modules = qr_modules(&text).unwrap();
    for background in [255u8, 128, 16] {
        let video = picture(W, H, background, 128);
        let out = burned_uyvy(&video, W, H, 1800, GEN_TS_NS).unwrap();
        assert!(
            video == picture(W, H, background, 128),
            "the picture itself is never written"
        );
        assert!(out[..] == expected(&video, W, H, &modules)[..]);
        assert_eq!(
            decode(&out, W, (1720, 0, 200, 200)),
            vec![text.clone()],
            "background luma {background}"
        );
        // The same luma as the NV12 canvas burn, pixel for pixel.
        let (canvas, layout) = nv12(W, H, background);
        let nv12_out = burned(&canvas, layout, 1800, GEN_TS_NS).unwrap();
        let uyvy_luma: Vec<u8> = out.iter().skip(1).step_by(2).copied().collect();
        assert!(
            uyvy_luma[..] == nv12_out[..(W * H) as usize],
            "background luma {background}"
        );
    }
    let video = picture(W, 200, 255, 128);
    assert!(burned_uyvy(&video, W, 200, 1800, 0).is_none(), "no room");
}
