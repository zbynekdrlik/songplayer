//! Tests for the BT.709 limited → full conversion (#223 S1a).

use super::{BT709_LIMITED_TO_FULL, KB, KR, matrix_f32};

/// 8-bit Y'CbCr → 8-bit RGB through the f32 rows, as the shader applies them
/// (saturated, then rounded).
fn convert(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let yuv1 = [
        f64::from(y) / 255.0,
        f64::from(cb) / 255.0,
        f64::from(cr) / 255.0,
        1.0,
    ];
    matrix_f32().map(|row| {
        let sum: f64 = row.iter().zip(yuv1).map(|(&m, v)| f64::from(m) * v).sum();
        (sum.clamp(0.0, 1.0) * 255.0).round() as u8
    })
}

#[test]
fn the_rows_follow_from_the_bt709_weights() {
    let kg = 1.0 - KR - KB;
    let ys = 255.0 / 219.0;
    let cs = 255.0 / 224.0;
    let (y0, c0) = (-16.0 / 219.0, -128.0 / 224.0);
    let rv = 2.0 * (1.0 - KR);
    let gu = -2.0 * KB * (1.0 - KB) / kg;
    let gv = -2.0 * KR * (1.0 - KR) / kg;
    let bu = 2.0 * (1.0 - KB);
    let derived = [
        [ys, 0.0, rv * cs, y0 + rv * c0],
        [ys, gu * cs, gv * cs, y0 + (gu + gv) * c0],
        [ys, bu * cs, 0.0, y0 + bu * c0],
    ];
    for (row, (got, want)) in BT709_LIMITED_TO_FULL.iter().zip(derived).enumerate() {
        for (col, (g, w)) in got.iter().zip(want).enumerate() {
            assert!((g - w).abs() < 1e-12, "row {row} col {col}: {g} vs {w}");
        }
    }
}

#[test]
fn the_shader_rows_are_the_matrix_rounded_to_f32() {
    let rows = matrix_f32();
    assert_eq!(rows[0], [1.164_383_5, 0.0, 1.792_741_1, -0.972_945_1]);
    for (row, (got, want)) in rows.iter().zip(BT709_LIMITED_TO_FULL).enumerate() {
        for (col, (&g, w)) in got.iter().zip(want).enumerate() {
            assert_eq!(g, w as f32, "row {row} col {col}");
        }
    }
}

/// One colour bar: its name, its limited-range Y'CbCr codes, and the two RGB
/// codes the table pins for it.
type ColourBar = (&'static str, [u8; 3], [u8; 3], [u8; 3]);

/// The known-value table: BT.709's 100 % colour bars in 8-bit limited range
/// (each Y'CbCr rounded to the code, so each channel is the ideal value
/// ±1), plus black, white and mid grey. The exact codes are pinned from a
/// scratch model of the same arithmetic.
#[test]
fn the_bt709_colour_bars_convert_to_their_rgb() {
    let table: [ColourBar; 9] = [
        ("black", [16, 128, 128], [0, 0, 0], [0, 0, 0]),
        ("white", [235, 128, 128], [255, 255, 255], [255, 255, 255]),
        ("grey", [126, 128, 128], [128, 128, 128], [128, 128, 128]),
        ("red", [63, 102, 240], [255, 0, 0], [255, 1, 0]),
        ("green", [173, 42, 26], [0, 255, 0], [0, 255, 1]),
        ("blue", [32, 240, 118], [0, 0, 255], [1, 0, 255]),
        ("yellow", [219, 16, 138], [255, 255, 0], [254, 255, 0]),
        ("cyan", [188, 154, 16], [0, 255, 255], [0, 254, 255]),
        ("magenta", [78, 214, 230], [255, 0, 255], [255, 0, 254]),
    ];
    for (name, [y, cb, cr], ideal, exact) in table {
        let rgb = convert(y, cb, cr);
        assert_eq!(rgb, exact, "{name}");
        for (got, want) in rgb.iter().zip(ideal) {
            assert!(
                got.abs_diff(want) <= 1,
                "{name}: {rgb:?} vs ideal {ideal:?}"
            );
        }
    }
}

#[test]
fn values_outside_the_limited_range_saturate() {
    assert_eq!(convert(255, 128, 128), [255, 255, 255]);
    assert_eq!(convert(0, 128, 128), [0, 0, 0]);
}
