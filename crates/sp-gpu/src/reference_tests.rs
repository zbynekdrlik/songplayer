//! Tests for the CPU model of the compositor (#223 S1a). Every expected
//! value comes from a scratch Python model of the same steps (the
//! rust-workspace.md no-compile discipline).

use sp_core::fit::Placement;

use super::{layer_rgb, pixel, sample, taps, unorm8, yuv_to_rgb};
use crate::composition::{Composition, Layer, Slot};
use crate::picture::{Nv12Picture, Plane};

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
        picture,
        place: Placement { w, h, off_x, off_y },
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
    // The outgoing side is stored first (blue 37), then the incoming one is
    // added to the stored value: one rounding of the sum would give 38.
    assert_eq!(pixel(&layers, 4, 2), [37, 77, 107, 255]);
}

#[test]
fn a_composed_picture_is_letterboxed_in_the_4k_canvas() {
    // 4×2 (2:1) → 3840×1920 at row 120.
    let layers = Composition::Picture(picture_a()).layers();
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

#[test]
fn a_composed_fade_adds_each_side_where_its_quad_reaches() {
    // A (4×2) at 1 − 64/256 over rows 120..2040; B (2×2) at 64/256 over
    // columns 840..3000.
    let layers = Composition::Fade {
        from: Some(picture_a()),
        to: Some(picture_b()),
        weight_q8: 64,
    }
    .layers();
    assert_eq!(pixel(&layers, 100, 50), [0, 0, 0, 255], "neither");
    assert_eq!(pixel(&layers, 1000, 50), [33, 50, 64, 255], "to only");
    assert_eq!(pixel(&layers, 100, 1000), [0, 4, 38, 255], "from only");
    assert_eq!(pixel(&layers, 1000, 1000), [24, 67, 120, 255], "both");
    assert_eq!(pixel(&layers, 2999, 2039), [210, 128, 64, 255], "both");
    assert_eq!(pixel(&layers, 3000, 1000), [191, 134, 26, 255], "from only");
    assert_eq!(pixel(&layers, 3839, 2159), [0, 0, 0, 255], "neither");
}
