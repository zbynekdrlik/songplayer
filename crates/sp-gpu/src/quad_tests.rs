//! Tests for the quad constants (#223 S1a).

use sp_core::fit::Placement;

use super::{QUAD_CONSTANTS_BYTES, QuadConstants, ndc_rect};
use crate::color::matrix_f32;

fn place(w: u32, h: u32, off_x: u32, off_y: u32) -> Placement {
    Placement { w, h, off_x, off_y }
}

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
fn a_quad_s_constants_are_its_canvas_rect_the_matrix_and_its_weight() {
    let constants = QuadConstants::new(place(2880, 2160, 480, 0), 0.25);
    assert_eq!(constants.rect, [-0.75, 1.0, 0.75, -1.0]);
    assert_eq!(constants.matrix, matrix_f32());
    assert_eq!(constants.weight, 0.25);
}

#[test]
fn the_constant_buffer_is_five_registers_in_the_hlsl_order() {
    let constants = QuadConstants {
        rect: [1.0, 2.0, 3.0, 4.0],
        matrix: [
            [5.0, 6.0, 7.0, 8.0],
            [9.0, 10.0, 11.0, 12.0],
            [13.0, 14.0, 15.0, 16.0],
        ],
        weight: 17.0,
    };
    let bytes = constants.to_bytes();
    assert_eq!(bytes.len(), QUAD_CONSTANTS_BYTES);
    let floats: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let mut want: Vec<f32> = (1..=17).map(|v| v as f32).collect();
    want.extend([0.0, 0.0, 0.0]);
    assert_eq!(floats, want);
}
