//! Tests for the quad constants (#223 S1a; #267: the part of the picture a
//! quad shows).

use sp_core::fit::{Fill, Placement, fill_width};

use super::{QUAD_CONSTANTS_BYTES, QuadConstants, ndc_rect, uv_in_texture, uv_rect};
use crate::color::matrix_f32;

fn place(w: u32, h: u32, off_x: u32, off_y: u32) -> Placement {
    Placement { w, h, off_x, off_y }
}

const WHOLE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

#[test]
fn a_full_canvas_quad_spans_the_whole_ndc_square() {
    assert_eq!(
        ndc_rect(place(3840, 2160, 0, 0), 3840, 2160),
        [-1.0, 1.0, 1.0, -1.0]
    );
}

#[test]
fn letterboxed_quads_stop_at_their_bars() {
    // 21:9: rows 270..1890 of 2160.
    assert_eq!(
        ndc_rect(place(3840, 1620, 0, 270), 3840, 2160),
        [-1.0, 0.75, 1.0, -0.75]
    );
    // 4:3: columns 480..3360 of 3840.
    assert_eq!(
        ndc_rect(place(2880, 2160, 480, 0), 3840, 2160),
        [-0.75, 1.0, 0.75, -1.0]
    );
}

#[test]
fn an_off_centre_quad_maps_each_edge_on_its_own_axis() {
    // Columns 1920..2880, rows 1080..1620: the lower right quarter's corner.
    assert_eq!(
        ndc_rect(place(960, 540, 1920, 1080), 3840, 2160),
        [0.0, 0.0, 0.5, -0.5]
    );
    // A 4×8 target: x = 2·px/4 − 1, y = 1 − 2·py/8.
    assert_eq!(ndc_rect(place(2, 2, 1, 1), 4, 8), [-0.5, 0.75, 0.5, 0.25]);
}

#[test]
fn a_whole_picture_shows_all_of_its_texture() {
    assert_eq!(uv_rect(Fill::whole(place(2880, 2160, 480, 0))), WHOLE);
    assert_eq!(uv_rect(Fill::whole(place(3840, 1620, 0, 270))), WHOLE);
    // A 21:9 picture under the fill rule is its letterbox, nothing cut off.
    assert_eq!(uv_rect(fill_width(2560, 1080, 3840, 2160)), WHOLE);
}

/// #267: a picture filling the target's width shows rows
/// `crop_top..crop_top + h` of its scaled rows, every column.
#[test]
fn a_cropped_picture_shows_the_middle_of_its_rows() {
    // 4:3 → 2880 scaled rows, 360..2520 shown.
    assert_eq!(
        uv_rect(fill_width(2880, 2160, 3840, 2160)),
        [0.0, 0.125, 1.0, 0.875]
    );
    // 1:1 → 3840 rows, 840..3000 shown.
    assert_eq!(
        uv_rect(fill_width(1080, 1080, 3840, 2160)),
        [0.0, 0.218_75, 1.0, 0.781_25]
    );
    // 9:16 → 6826 rows, 2332..4492 shown.
    assert_eq!(
        uv_rect(fill_width(1080, 1920, 3840, 2160)),
        [0.0, 0.341_634_93, 1.0, 0.658_072_05]
    );
    // 5×7 into 9×3 → 12 rows, 4..6 shown.
    assert_eq!(
        uv_rect(fill_width(5, 7, 9, 3)),
        [0.0, 0.333_333_34, 1.0, 0.5]
    );
}

#[test]
fn a_degenerate_picture_shows_no_rows() {
    // No scaled rows: no division by zero, an empty band (its quad covers
    // nothing anyway).
    assert_eq!(uv_rect(fill_width(0, 2, 10, 14)), [0.0, 0.0, 1.0, 0.0]);
}

#[test]
fn a_quad_s_constants_are_its_canvas_rect_its_crop_the_matrix_and_its_weight() {
    let crop = [0.0, 0.125, 1.0, 0.875];
    let constants = QuadConstants::new(place(2880, 2160, 480, 0), crop, 0.25, 3840, 2160);
    assert_eq!(constants.rect, [-0.75, 1.0, 0.75, -1.0]);
    assert_eq!(constants.uv_rect, crop);
    assert_eq!(constants.matrix, matrix_f32());
    assert_eq!(constants.weight, 0.25);
}

/// #239: the rectangle is in the compositor's own target, x across its
/// width and y down its height (a 4:3 picture in `SP-program`'s 1920×1080,
/// then the same place read in another target).
#[test]
fn a_quad_s_rect_is_in_its_own_target() {
    let pillarbox = place(1440, 1080, 240, 0);
    let fhd = QuadConstants::new(pillarbox, WHOLE, 0.5, 1920, 1080);
    assert_eq!(fhd.rect, [-0.75, 1.0, 0.75, -1.0]);
    assert_eq!(fhd.weight, 0.5);
    let wide = QuadConstants::new(place(960, 270, 480, 135), WHOLE, 1.0, 1920, 1080);
    assert_eq!(wide.rect, [-0.5, 0.75, 0.5, 0.25]);
    // The same place in MAX's 3840×2160 canvas lies in its upper left.
    let max = QuadConstants::new(pillarbox, WHOLE, 0.5, 3840, 2160);
    assert_eq!(max.rect, [-0.875, 1.0, -0.125, 0.0]);
}

#[test]
fn the_constant_buffer_is_six_registers_in_the_hlsl_order() {
    let constants = QuadConstants {
        rect: [1.0, 2.0, 3.0, 4.0],
        uv_rect: [5.0, 6.0, 7.0, 8.0],
        matrix: [
            [9.0, 10.0, 11.0, 12.0],
            [13.0, 14.0, 15.0, 16.0],
            [17.0, 18.0, 19.0, 20.0],
        ],
        weight: 21.0,
    };
    let bytes = constants.to_bytes();
    assert_eq!(bytes.len(), QUAD_CONSTANTS_BYTES);
    assert_eq!(QUAD_CONSTANTS_BYTES, 96, "six float4 registers");
    let floats: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let mut want: Vec<f32> = (1..=21).map(|v| v as f32).collect();
    want.extend([0.0, 0.0, 0.0]);
    assert_eq!(floats, want);
}

/// #279 P2: an uploaded picture's texture is its own size: the texture
/// coordinates stay exactly what `uv_rect` gave (bit for bit, so every pin
/// of the CPU reference holds).
#[test]
fn a_texture_of_the_picture_s_size_keeps_the_texture_coordinates() {
    let crop = [0.0, 0.125, 1.0, 0.875];
    assert_eq!(uv_in_texture(crop, (1920, 1080), (1920, 1080)), crop);
    assert_eq!(uv_in_texture(WHOLE, (1440, 1080), (1440, 1080)), WHOLE);
}

/// #279 P2: an odd picture in a ring slot's even texture spans its own
/// columns and rows: u scales by 1919/1920, v by 1079/1080.
#[test]
fn an_odd_picture_in_an_even_slot_spans_its_own_texels() {
    let (su, sv) = (1919.0_f32 / 1920.0, 1079.0_f32 / 1080.0);
    assert_eq!(
        uv_in_texture(WHOLE, (1919, 1079), (1920, 1080)),
        [0.0, 0.0, su, sv]
    );
    let inner = [0.25, 0.5, 0.75, 1.0];
    assert_eq!(
        uv_in_texture(inner, (1919, 1079), (1920, 1080)),
        [0.25 * su, 0.5 * sv, 0.75 * su, sv]
    );
    assert_eq!(
        uv_in_texture(inner, (1920, 1079), (1920, 1080)),
        [0.25, 0.5 * sv, 0.75, sv],
        "each axis by its own sides"
    );
    assert_eq!(
        uv_in_texture(WHOLE, (0, 0), (0, 0)),
        [0.0; 4],
        "an empty texture reads no NaN"
    );
}

/// The shader is compiled only on Windows (`D3DCompile`, at runtime), so
/// this Linux test reads its source: `cbuffer Quad` declares the registers
/// in `to_bytes`' order, and each corner's texture coordinate is
/// `uv_rect`'s edge on the corner's side, as its position is `rect`'s.
#[test]
fn the_hlsl_cbuffer_has_the_registers_in_the_same_order() {
    let hlsl = include_str!("compose.hlsl");
    let at = |declaration: &str| {
        hlsl.find(declaration)
            .unwrap_or_else(|| panic!("compose.hlsl declares `{declaration}`"))
    };
    let order = [
        "float4 rect;",
        "float4 uv_rect;",
        "float4 to_r;",
        "float4 to_g;",
        "float4 to_b;",
        "float4 weight;",
    ]
    .map(at);
    assert!(order.windows(2).all(|p| p[0] < p[1]), "{order:?}");
    assert!(
        hlsl.contains(
            "corner.uv = float2(t.x > 0.5 ? uv_rect.z : uv_rect.x, \
             t.y > 0.5 ? uv_rect.w : uv_rect.y);"
        ),
        "the corners take uv_rect's edges"
    );
}
