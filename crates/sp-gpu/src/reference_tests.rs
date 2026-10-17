//! Tests for the CPU model of the compositor (#223 S1a). Every expected
//! value comes from a scratch Python model of the same steps (the
//! rust-workspace.md no-compile discipline).

use sp_core::fit::Placement;

use super::{covers, layer_rgb, pixel, sample, taps, tolerance, unorm8, yuv_to_rgb};
use crate::composition::{Composition, FHD_FIT, FHD_TARGET, Layer, MAX_TARGET, Side, Slot};
use crate::picture::{Nv12Picture, Plane};
use crate::ring::FakeRing;

const PAD: u8 = 0xEE;

/// A 4×2 picture, stride 6 (two padding bytes per row that must never be
/// read): luma rows [16 60 120 235] and [40 90 200 30], one chroma row of
/// two U/V pairs (100, 150) and (200, 60).
const A: [u8; 18] = [
    16, 60, 120, 235, PAD, PAD, //
    40, 90, 200, 30, PAD, PAD, //
    100, 150, 200, 60, PAD, PAD,
];

/// A 2×2 picture, stride 4: luma [200 50] and [120 180], one U/V pair
/// (90, 170).
const B: [u8; 12] = [
    200, 50, PAD, PAD, //
    120, 180, PAD, PAD, //
    90, 170, PAD, PAD,
];

fn picture_a() -> Nv12Picture<'static> {
    Nv12Picture {
        id: 1,
        width: 4,
        height: 2,
        stride: 6,
        data: &A,
    }
}

fn picture_b() -> Nv12Picture<'static> {
    Nv12Picture {
        id: 2,
        width: 2,
        height: 2,
        stride: 4,
        data: &B,
    }
}

fn layer(
    picture: Nv12Picture<'static>,
    (w, h, off_x, off_y): (u32, u32, u32, u32),
    weight: f32,
) -> Layer<'static> {
    Layer {
        slot: Slot::Outgoing,
        side: Side::Cpu(picture),
        place: Placement { w, h, off_x, off_y },
        uv_rect: [0.0, 0.0, 1.0, 1.0],
        weight,
    }
}

fn close(got: f64, want: f64) -> bool {
    (got - want).abs() < 1e-12
}

#[test]
fn the_taps_follow_the_texel_centre_rule() {
    // A 4-texel axis drawn 8 pixels wide: u = (d + ½)/8, t = 4u − ½.
    assert_eq!(taps(1.0 / 16.0, 4), (0, 0, 0.75)); // t = −0.25: the edge
    assert_eq!(taps(3.0 / 16.0, 4), (0, 1, 0.25)); // t = 0.25
    assert_eq!(taps(9.0 / 16.0, 4), (1, 2, 0.75)); // t = 1.75
    assert_eq!(taps(15.0 / 16.0, 4), (3, 3, 0.25)); // t = 3.25: clamped
    assert_eq!(taps(0.5, 1), (0, 0, 0.0));
}

#[test]
fn a_bilinear_sample_weights_the_four_texels() {
    let luma = Plane {
        width: 4,
        height: 2,
        offset: 0,
    };
    let chroma = Plane {
        width: 2,
        height: 1,
        offset: 12,
    };
    let (u, v) = (0.4375, 0.625);
    // Luma: texels (1, 0) 60, (2, 0) 120, (1, 1) 90, (2, 1) 200 at
    // fx 0.25, fy 0.75: 75·0.25 + 117.5·0.75 = 106.875.
    assert!(close(sample(&A, luma, 6, 1, 0, u, v), 106.875 / 255.0));
    // U: 100·0.625 + 200·0.375 = 137.5; V: 150·0.625 + 60·0.375 = 116.25.
    assert!(close(sample(&A, chroma, 6, 2, 0, u, v), 137.5 / 255.0));
    assert!(close(sample(&A, chroma, 6, 2, 1, u, v), 116.25 / 255.0));
}

#[test]
fn a_sample_past_the_last_texel_reads_the_edge_never_the_padding() {
    let luma = Plane {
        width: 4,
        height: 2,
        offset: 0,
    };
    // u = 15/16 → texels 3 and 3 (235); v = 1/8 → row 0 only.
    assert!(close(
        sample(&A, luma, 6, 1, 0, 15.0 / 16.0, 0.125),
        235.0 / 255.0
    ));
    // v = 7/8 → row 1 only (30).
    assert!(close(
        sample(&A, luma, 6, 1, 0, 15.0 / 16.0, 0.875),
        30.0 / 255.0
    ));
}

#[test]
fn the_colour_rows_are_applied_and_saturated() {
    let rgb = yuv_to_rgb(106.875 / 255.0, 137.5 / 255.0, 116.25 / 255.0);
    let want = [0.332_347_604_282, 0.431_565_406_276, 0.493_651_602_490];
    for (got, want) in rgb.iter().zip(want) {
        assert!((got - want).abs() < 1e-9, "{rgb:?}");
    }
    // White in every plane: R and B saturate, G does not.
    let white = yuv_to_rgb(1.0, 1.0, 1.0);
    assert_eq!(white[0], 1.0);
    assert!((white[1] - 0.719_708_263_874).abs() < 1e-9, "{white:?}");
    assert_eq!(white[2], 1.0);
    // Zero in every plane: R and B clamp at 0, G is the row's constant.
    let zero = yuv_to_rgb(0.0, 0.0, 0.0);
    assert_eq!(zero[0], 0.0);
    assert!((zero[1] - 0.301_482_677_460).abs() < 1e-9, "{zero:?}");
    assert_eq!(zero[2], 0.0);
}

#[test]
fn a_stored_value_rounds_to_the_nearest_code() {
    assert_eq!(unorm8(0.0), 0);
    assert_eq!(unorm8(1.0), 255);
    assert_eq!(unorm8(-0.5), 0);
    assert_eq!(unorm8(1.5), 255);
    assert_eq!(unorm8(127.5 / 255.0), 128);
    assert_eq!(unorm8(127.4 / 255.0), 127);
    assert_eq!(unorm8(63.75 / 255.0), 64);
}

#[test]
fn a_quad_covers_the_pixels_whose_centres_are_inside_its_rectangle() {
    // Picture A drawn 8×4 at (2, 2): pixels 2..10 × 2..6.
    let quad = layer(picture_a(), (8, 4, 2, 2), 1.0);
    let inside = layer_rgb(&quad, 5, 4).expect("(5, 4) is inside");
    let want = yuv_to_rgb(106.875 / 255.0, 137.5 / 255.0, 116.25 / 255.0);
    for (got, want) in inside.iter().zip(want) {
        assert!(close(*got, want), "{inside:?}");
    }
    assert!(layer_rgb(&quad, 9, 5).is_some(), "the last pixel");
    assert_eq!(layer_rgb(&quad, 1, 4), None, "left of the quad");
    assert_eq!(layer_rgb(&quad, 10, 4), None, "right of the quad");
    assert_eq!(layer_rgb(&quad, 5, 1), None, "above the quad");
    assert_eq!(layer_rgb(&quad, 5, 6), None, "below the quad");
    assert_eq!(layer_rgb(&quad, 10, 6), None, "past both edges");
}

/// #267: a quad shows the `uv_rect` part of its picture, the coordinates
/// interpolated across it from its edges: picture A's middle half on each
/// axis, drawn 8×4 at (2, 2).
#[test]
fn a_cropped_quad_samples_only_its_part_of_the_picture() {
    let cropped = |uv_rect| Layer {
        uv_rect,
        ..layer(picture_a(), (8, 4, 2, 2), 1.0)
    };
    let middle = cropped([0.25, 0.25, 0.75, 0.75]);
    assert_eq!(pixel(&[middle], 2, 2), [0, 32, 65, 255], "the first pixel");
    assert_eq!(pixel(&[middle], 5, 4), [146, 119, 82, 255]);
    assert_eq!(
        pixel(&[middle], 9, 5),
        [255, 163, 32, 255],
        "the last pixel"
    );
    // Each axis reads its own edges: the rows only, then the columns only.
    let rows = cropped([0.0, 0.25, 1.0, 0.75]);
    assert_eq!(pixel(&[rows], 5, 4), [120, 104, 79, 255]);
    let columns = cropped([0.25, 0.0, 0.75, 1.0]);
    assert_eq!(pixel(&[columns], 5, 4), [153, 126, 89, 255]);
    // The whole picture reads otherwise there (`one_layer_is_…` below).
    let whole = layer(picture_a(), (8, 4, 2, 2), 1.0);
    assert_eq!(pixel(&[whole], 5, 4), [126, 110, 85, 255]);
    // Coverage is the place's alone.
    assert_eq!(layer_rgb(&middle, 1, 2), None, "left of the quad");
    assert_eq!(layer_rgb(&middle, 10, 5), None, "right of the quad");
}

/// #279 P2: the model reads bytes, so a ring frame's layer adds nothing to
/// it (the WARP pins take the reference of the CPU picture written into the
/// slot); its coverage, and so the tolerance, is its place's as for any
/// layer.
#[test]
fn a_ring_frame_s_layer_is_not_modelled() {
    let ring = FakeRing::new(1);
    let frame = ring
        .frame(4, 2, std::time::Duration::ZERO)
        .expect("a free slot");
    let quad = Layer {
        side: Side::Gpu(&frame),
        ..layer(picture_a(), (8, 4, 2, 2), 1.0)
    };
    assert_eq!(layer_rgb(&quad, 5, 4), None);
    assert_eq!(pixel(&[quad], 5, 4), [0, 0, 0, 255]);
    assert!(covers(&quad, 5, 4));
    assert_eq!(tolerance(&[quad], 5, 4), 1);
}

#[test]
fn one_layer_is_its_rgb_stored_as_bgra() {
    let quad = layer(picture_a(), (8, 4, 2, 2), 1.0);
    // RGB (85, 110, 126) → BGRA.
    assert_eq!(pixel(&[quad], 5, 4), [126, 110, 85, 255]);
    assert_eq!(pixel(&[quad], 9, 5), [168, 37, 0, 255]);
    assert_eq!(pixel(&[quad], 0, 0), [0, 0, 0, 255], "outside: the black");
    assert_eq!(pixel(&[], 5, 4), [0, 0, 0, 255], "no layer: the black");
}

#[test]
fn two_layers_add_at_their_weights_stored_in_8_bits_between() {
    // A at 0.75 over 2..10 × 2..6, then B at 0.25 over 4..8 × 2..6.
    let from = layer(picture_a(), (8, 4, 2, 2), 0.75);
    let to = Layer {
        slot: Slot::Incoming,
        ..layer(picture_b(), (4, 4, 4, 2), 0.25)
    };
    let layers = [from, to];
    assert_eq!(pixel(&layers, 5, 4), [111, 116, 119, 255], "both");
    assert_eq!(pixel(&layers, 7, 3), [191, 147, 80, 255], "both");
    assert_eq!(pixel(&layers, 3, 3), [0, 11, 45, 255], "from only");
    assert_eq!(pixel(&layers, 9, 5), [126, 28, 0, 255], "from only");
    // The outgoing side alone is stored first (blue 4); the incoming one is
    // added to that stored value, giving 37. One rounding of the exact sum
    // would give 38.
    assert_eq!(pixel(&[from], 4, 2), [4, 27, 43, 255]);
    assert_eq!(pixel(&layers, 4, 2), [37, 77, 107, 255]);
}

/// #267: a picture wider than 16:9 keeps its letterbox under MAX's rule.
#[test]
fn a_composed_picture_is_letterboxed_in_the_4k_canvas() {
    // 4×2 (2:1) → 3840×1920 at row 120.
    let layers = Composition::Picture(picture_a().into()).layers();
    assert_eq!(pixel(&layers, 0, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&layers, 0, 119), [0, 0, 0, 255], "the last bar row");
    assert_eq!(pixel(&layers, 0, 120), [0, 0, 39, 255], "texel (0, 0)");
    assert_eq!(pixel(&layers, 1920, 1080), [165, 126, 77, 255]);
    assert_eq!(
        pixel(&layers, 3839, 2039),
        [168, 37, 0, 255],
        "texel (3, 1)"
    );
    assert_eq!(pixel(&layers, 0, 2040), [0, 0, 0, 255], "the first bar row");
}

/// #267: a picture narrower than 16:9 fills MAX's whole canvas: B (2×2,
/// 1:1) scaled to 3840×3840, its rows 840..3000 shown, so no pixel is a
/// bar (each within one code: one quad covers it).
#[test]
fn a_composed_narrow_picture_fills_the_4k_canvas() {
    let layers = Composition::Picture(picture_b().into()).layers();
    assert_eq!(pixel(&layers, 0, 0), [134, 200, 255, 255], "the top left");
    assert_eq!(pixel(&layers, 0, 1079), [87, 153, 243, 255]);
    assert_eq!(pixel(&layers, 1920, 1080), [61, 127, 217, 255]);
    assert_eq!(pixel(&layers, 3839, 0), [0, 25, 115, 255], "the top right");
    assert_eq!(pixel(&layers, 3839, 2159), [111, 177, 255, 255]);
    assert_eq!(tolerance(&layers, 0, 0), 1, "a picture, not a bar");
    assert_eq!(tolerance(&layers, 3839, 2159), 1, "a picture, not a bar");
}

#[test]
fn a_composed_fade_adds_each_side_where_its_quad_reaches() {
    // A (4×2) at 1 − 64/256 over rows 120..2040; B (2×2) at 64/256 over the
    // whole canvas (#267: it fills MAX's width, rows 840..3000 of its 3840
    // scaled rows shown).
    let layers = Composition::Fade {
        from: Some(picture_a().into()),
        to: Some(picture_b().into()),
        weight_q8: 64,
    }
    .layers();
    assert_eq!(pixel(&layers, 100, 50), [33, 50, 64, 255], "to only");
    assert_eq!(pixel(&layers, 1000, 50), [33, 49, 64, 255], "to only");
    assert_eq!(pixel(&layers, 0, 119), [33, 50, 64, 255], "to only");
    assert_eq!(pixel(&layers, 0, 120), [33, 50, 94, 255], "both");
    assert_eq!(pixel(&layers, 100, 1000), [23, 43, 100, 255], "both");
    assert_eq!(pixel(&layers, 1000, 1000), [22, 66, 119, 255], "both");
    assert_eq!(pixel(&layers, 2999, 2039), [210, 128, 64, 255], "both");
    assert_eq!(pixel(&layers, 3000, 1000), [198, 158, 72, 255], "both");
    assert_eq!(pixel(&layers, 3839, 2039), [154, 72, 64, 255], "both");
    assert_eq!(pixel(&layers, 3839, 2040), [28, 44, 64, 255], "to only");
    assert_eq!(pixel(&layers, 3839, 2159), [28, 44, 64, 255], "to only");
}

/// #239: each layer carries its place in its own target
/// (`Composition::layers_in`), so the model pins `SP-program`'s 1920×1080
/// like MAX's canvas: the same 2:1 picture letterboxed at row 60, its
/// corners and centre the same texels as in 4K.
#[test]
fn a_composed_picture_is_letterboxed_in_the_fhd_target() {
    // 4×2 (2:1) → 1920×960 at row 60.
    let layers = Composition::Picture(picture_a().into()).layers_in(1920, 1080, FHD_FIT);
    assert_eq!(pixel(&layers, 0, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&layers, 0, 59), [0, 0, 0, 255], "the last bar row");
    assert_eq!(pixel(&layers, 0, 60), [0, 0, 39, 255], "texel (0, 0)");
    assert_eq!(pixel(&layers, 960, 540), [165, 126, 77, 255]);
    assert_eq!(
        pixel(&layers, 1919, 1019),
        [168, 37, 0, 255],
        "texel (3, 1)"
    );
    assert_eq!(pixel(&layers, 0, 1020), [0, 0, 0, 255], "the first bar row");
    assert_eq!(tolerance(&layers, 0, 59), 0, "a bar: exact");
    assert_eq!(tolerance(&layers, 0, 60), 1, "the picture");
}

/// #239: a fade in the 1920×1080 target samples on its own pixel grid: a
/// pixel between texels reads other filter weights than its 4K twin.
#[test]
fn a_composed_fade_in_the_fhd_target_samples_its_own_grid() {
    // A (4×2) at 1 − 64/256 over rows 60..1020; B (2×2) at 64/256 over
    // columns 420..1500.
    let layers = Composition::Fade {
        from: Some(picture_a().into()),
        to: Some(picture_b().into()),
        weight_q8: 64,
    }
    .layers_in(1920, 1080, FHD_FIT);
    assert_eq!(pixel(&layers, 50, 25), [0, 0, 0, 255], "neither");
    assert_eq!(pixel(&layers, 500, 25), [33, 50, 64, 255], "to only");
    assert_eq!(pixel(&layers, 50, 500), [0, 4, 38, 255], "from only");
    assert_eq!(pixel(&layers, 500, 500), [24, 67, 120, 255], "both");
    assert_eq!(pixel(&layers, 1499, 1019), [210, 128, 64, 255], "both");
    // 4K's (3000, 1000) reads green 134: another sample point.
    assert_eq!(pixel(&layers, 1500, 500), [191, 133, 26, 255], "from only");
    assert_eq!(pixel(&layers, 1919, 1079), [0, 0, 0, 255], "neither");
    assert_eq!(pixel(&layers, 420, 60), [33, 60, 108, 255], "both, first");
    assert_eq!(tolerance(&layers, 420, 60), 2, "both, at their first pixel");
    assert_eq!(tolerance(&layers, 419, 59), 0, "neither, just outside");
}

#[test]
fn the_tolerance_is_one_code_per_layer_covering_the_pixel() {
    // The same fade: A over rows 120..2040, B (#267) over the whole canvas.
    let layers = Composition::Fade {
        from: Some(picture_a().into()),
        to: Some(picture_b().into()),
        weight_q8: 64,
    }
    .layers();
    assert_eq!(tolerance(&layers, 100, 50), 1, "to only: A's bar");
    assert_eq!(tolerance(&layers, 0, 119), 1, "to only, A's last bar row");
    assert_eq!(tolerance(&layers, 0, 120), 2, "both, A's first row");
    assert_eq!(tolerance(&layers, 1000, 1000), 2, "both");
    assert_eq!(tolerance(&layers, 3839, 2039), 2, "both, A's last pixel");
    assert_eq!(tolerance(&layers, 3839, 2040), 1, "to only, A's bar");
    assert_eq!(tolerance(&[], 1000, 1000), 0, "the black: exact");
    // `SP-program`'s target keeps the bars: there B is pillarboxed
    // (columns 420..1500), and a pixel neither side reaches is exact.
    let fhd = Composition::Fade {
        from: Some(picture_a().into()),
        to: Some(picture_b().into()),
        weight_q8: 64,
    }
    .layers_in(1920, 1080, FHD_FIT);
    assert_eq!(tolerance(&fhd, 50, 25), 0, "neither: exact");
    assert_eq!(tolerance(&fhd, 500, 25), 1, "to only");
    assert_eq!(tolerance(&fhd, 50, 500), 1, "from only");
}

/// #251 step 2: ONE composition drawn on both targets of one compositor:
/// its MAX layers give the 4K pins and its FHD layers the 1920×1080 ones
/// above, each target's bars and tolerance its own (the model needs no
/// second composition, nor a second upload).
#[test]
fn one_composition_gives_each_target_its_own_reference() {
    let fade = Composition::Fade {
        from: Some(picture_a().into()),
        to: Some(picture_b().into()),
        weight_q8: 64,
    };
    let max = fade.layers_on(MAX_TARGET);
    let fhd = fade.layers_on(FHD_TARGET);
    assert_eq!(pixel(&max, 0, 120), [33, 50, 94, 255], "MAX: both");
    assert_eq!(pixel(&max, 3839, 2040), [28, 44, 64, 255], "MAX: to only");
    assert_eq!(tolerance(&max, 1000, 1000), 2, "MAX: both");
    assert_eq!(pixel(&fhd, 50, 25), [0, 0, 0, 255], "FHD: neither");
    assert_eq!(pixel(&fhd, 500, 500), [24, 67, 120, 255], "FHD: both");
    assert_eq!(
        pixel(&fhd, 1500, 500),
        [191, 133, 26, 255],
        "FHD: from only"
    );
    assert_eq!(tolerance(&fhd, 50, 25), 0, "FHD: a bar, exact");
    assert_eq!(tolerance(&fhd, 500, 500), 2, "FHD: both");
}

#[test]
fn a_quad_covers_exactly_its_rectangle() {
    let quad = layer(picture_a(), (8, 4, 2, 2), 1.0);
    assert!(covers(&quad, 2, 2));
    assert!(covers(&quad, 9, 5));
    assert!(!covers(&quad, 1, 2));
    assert!(!covers(&quad, 10, 5));
    assert!(!covers(&quad, 9, 6));
}
