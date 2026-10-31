//! Tests for the UYVY pass's pure side (#279 P1a): the conversion rows, the
//! constant buffer and the shader source they must match, the CPU model of
//! a pixel pair and of a frame, the size rule, the readback map's error and
//! the staging ring. Every exact code below comes from a scratch model of
//! the same f32 arithmetic.

use super::{
    MAP_CALL, RGB_FULL_TO_BT709_LIMITED, STILL_DRAWING_CALL, StagingRing, UYVY_CONSTANTS_BYTES,
    UYVY_STAGING, UYVY_TOLERANCE, constants_bytes, map_failed, pack_pair, reference, rows_f32,
    uyvy_size,
};
use crate::color::{BT709_LIMITED_TO_FULL, KB, KR};
use crate::error::{DXGI_ERROR_DEVICE_REMOVED, GpuError};
use crate::probe::DXGI_ERROR_WAS_STILL_DRAWING;

/// An opaque BGRA pixel of full-range `r`, `g`, `b`.
fn bgra(r: u8, g: u8, b: u8) -> [u8; 4] {
    [b, g, r, 255]
}

#[test]
fn the_rows_follow_from_the_bt709_weights() {
    let kg = 1.0 - KR - KB;
    let sb = 224.0 / (255.0 * 2.0 * (1.0 - KB));
    let sr = 224.0 / (255.0 * 2.0 * (1.0 - KR));
    let derived = [
        [
            219.0 * KR / 255.0,
            219.0 * kg / 255.0,
            219.0 * KB / 255.0,
            16.0 / 255.0,
        ],
        [-KR * sb, -kg * sb, (1.0 - KB) * sb, 128.0 / 255.0],
        [(1.0 - KR) * sr, -kg * sr, -KB * sr, 128.0 / 255.0],
    ];
    for (row, (got, want)) in RGB_FULL_TO_BT709_LIMITED.iter().zip(derived).enumerate() {
        for (col, (g, w)) in got.iter().zip(want).enumerate() {
            assert!((g - w).abs() < 1e-12, "row {row} col {col}: {g} vs {w}");
        }
    }
}

/// The 4×4 affine matrix of three rows over `(a, b, c, 1)`.
fn affine(rows: [[f64; 4]; 3]) -> [[f64; 4]; 4] {
    [rows[0], rows[1], rows[2], [0.0, 0.0, 0.0, 1.0]]
}

fn product(a: [[f64; 4]; 4], b: [[f64; 4]; 4]) -> [[f64; 4]; 4] {
    let mut out = [[0.0; 4]; 4];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            let mut sum = 0.0;
            for (k, a_ik) in a[i].iter().enumerate() {
                sum += a_ik * b[k][j];
            }
            *cell = sum;
        }
    }
    out
}

/// The conversion is the inverse of the compositor's own BT.709 limited →
/// full rows, both ways round: RGB → Y′CbCr → RGB and Y′CbCr → RGB →
/// Y′CbCr are the identity within 1e-9.
#[test]
fn the_rows_invert_the_limited_to_full_matrix() {
    let to_limited = affine(RGB_FULL_TO_BT709_LIMITED);
    let to_full = affine(BT709_LIMITED_TO_FULL);
    for (what, m) in [
        ("limited after full", product(to_limited, to_full)),
        ("full after limited", product(to_full, to_limited)),
    ] {
        for (i, row) in m.iter().enumerate() {
            for (j, &cell) in row.iter().enumerate() {
                let identity = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (cell - identity).abs() < 1e-9,
                    "{what}: [{i}][{j}] = {cell}"
                );
            }
        }
    }
}

#[test]
fn the_shader_rows_are_the_matrix_rounded_to_f32() {
    let rows = rows_f32();
    assert_eq!(
        rows[0],
        [0.182_585_88, 0.614_230_6, 0.062_007_06, 0.062_745_1]
    );
    assert_eq!(
        rows[1],
        [-0.100_643_73, -0.338_571_97, 0.439_215_7, 0.501_960_8]
    );
    assert_eq!(
        rows[2],
        [0.439_215_7, -0.398_942_17, -0.040_273_525, 0.501_960_8]
    );
    for (row, (got, want)) in rows.iter().zip(RGB_FULL_TO_BT709_LIMITED).enumerate() {
        for (col, (&g, w)) in got.iter().zip(want).enumerate() {
            assert_eq!(g, w as f32, "row {row} col {col}");
        }
    }
}

#[test]
fn the_constant_buffer_is_the_three_rows_in_order() {
    let bytes = constants_bytes();
    assert_eq!(UYVY_CONSTANTS_BYTES, 48, "three float4 registers");
    assert_eq!(bytes.len(), UYVY_CONSTANTS_BYTES);
    let floats: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let want: Vec<f32> = rows_f32().into_iter().flatten().collect();
    assert_eq!(floats, want);
    assert_eq!(floats[4], -0.100_643_73, "register 1 is Cb's row");
}

/// The shader compiles only on Windows (`D3DCompile`, at runtime), so this
/// Linux test reads its source: `cbuffer Uyvy` declares the rows in
/// `constants_bytes`' order (Y′, Cb, Cr), each texel reads source pixels
/// 2x and 2x + 1 of its row, and it writes (U, Y0, V, Y1) as
/// `pack_pair` returns them.
#[test]
fn the_hlsl_cbuffer_loads_and_output_match_the_cpu_model() {
    let hlsl = include_str!("uyvy.hlsl");
    let at = |declaration: &str| {
        hlsl.find(declaration)
            .unwrap_or_else(|| panic!("uyvy.hlsl declares `{declaration}`"))
    };
    let order = ["float4 to_y;", "float4 to_u;", "float4 to_v;"].map(at);
    assert!(order.windows(2).all(|p| p[0] < p[1]), "{order:?}");
    assert!(
        hlsl.contains("float3 left = source_bgra.Load(int3(2 * texel.x, texel.y, 0)).rgb;"),
        "the left pixel is source column 2x"
    );
    assert!(
        hlsl.contains("float3 right = source_bgra.Load(int3(2 * texel.x + 1, texel.y, 0)).rgb;"),
        "the right pixel is source column 2x + 1"
    );
    assert!(
        hlsl.contains("float4 mean = float4((left + right) * 0.5, 1.0);"),
        "chroma of the pair's average RGB"
    );
    assert!(
        hlsl.contains(
            "return saturate(float4(dot(to_u, mean), dot(to_y, float4(left, 1.0)), \
             dot(to_v, mean), dot(to_y, float4(right, 1.0))));"
        ),
        "the texel is (U, Y0, V, Y1)"
    );
}

/// One colour bar: its name, its full-range RGB, and its UYVY pair when
/// both pixels are that colour.
type Bar = (&'static str, [u8; 3], [u8; 4]);

/// BT.709's 100 % colour bars, black, white and mid grey: a pair of one
/// colour gives `[Cb, Y′, Cr, Y′]`, the limited-range codes
/// `color_tests.rs` converts back to the same bars (each within ±1 of
/// the ideal).
#[test]
fn the_bt709_colour_bars_pack_to_their_limited_range_codes() {
    let table: [Bar; 9] = [
        ("black", [0, 0, 0], [128, 16, 128, 16]),
        ("white", [255, 255, 255], [128, 235, 128, 235]),
        ("grey", [128, 128, 128], [128, 126, 128, 126]),
        ("red", [255, 0, 0], [102, 63, 240, 63]),
        ("green", [0, 255, 0], [42, 173, 26, 173]),
        ("blue", [0, 0, 255], [240, 32, 118, 32]),
        ("yellow", [255, 255, 0], [16, 219, 138, 219]),
        ("cyan", [0, 255, 255], [154, 188, 16, 188]),
        ("magenta", [255, 0, 255], [214, 78, 230, 78]),
    ];
    for (name, [r, g, b], want) in table {
        let pixel = bgra(r, g, b);
        assert_eq!(pack_pair(pixel, pixel), want, "{name}");
    }
}

/// Each pixel keeps its own luma (Y0 is the left one's, Y1 the right
/// one's); the chroma is the conversion of the pair's average RGB.
#[test]
fn a_pair_keeps_each_luma_and_shares_its_average_chroma() {
    // Red then blue: Y′ 63 and 32; the average (127.5, 0, 127.5).
    assert_eq!(
        pack_pair(bgra(255, 0, 0), bgra(0, 0, 255)),
        [171, 63, 179, 32]
    );
    assert_eq!(
        pack_pair(bgra(255, 255, 255), bgra(0, 0, 0)),
        [128, 235, 128, 16]
    );
    assert_eq!(
        pack_pair(bgra(0, 0, 0), bgra(255, 255, 255)),
        [128, 16, 128, 235]
    );
    // Green and magenta average to a grey: no chroma.
    assert_eq!(
        pack_pair(bgra(0, 255, 0), bgra(255, 0, 255)),
        [128, 173, 128, 78]
    );
    assert_eq!(
        pack_pair(bgra(200, 100, 50), bgra(10, 180, 240)),
        [134, 117, 112, 143]
    );
    assert_eq!(
        pack_pair(bgra(17, 34, 51), bgra(68, 85, 102)),
        [137, 43, 120, 87]
    );
}

#[test]
fn the_alpha_byte_is_never_read() {
    let opaque = pack_pair(bgra(200, 100, 50), bgra(10, 180, 240));
    let clear = pack_pair([50, 100, 200, 0], [240, 180, 10, 7]);
    assert_eq!(clear, opaque);
}

/// A 4×2 frame (two pairs a row) whose buffer holds a third row: the
/// reference packs `height` rows of `width / 2` pairs, each by `pack_pair`.
#[test]
fn the_reference_packs_each_row_s_pairs() {
    let pixels = [
        bgra(255, 0, 0),
        bgra(0, 0, 255),
        bgra(255, 255, 255),
        bgra(0, 0, 0),
        bgra(0, 255, 0),
        bgra(255, 0, 255),
        bgra(17, 34, 51),
        bgra(68, 85, 102),
        bgra(1, 2, 3),
        bgra(4, 5, 6),
        bgra(7, 8, 9),
        bgra(10, 11, 12),
    ];
    let frame: Vec<u8> = pixels.iter().flatten().copied().collect();
    let uyvy = reference(&frame, 4, 2);
    assert_eq!(
        uyvy,
        [
            171, 63, 179, 32, 128, 235, 128, 16, // row 0
            128, 173, 128, 78, 137, 43, 120, 87, // row 1
        ]
    );
    assert_eq!(uyvy.len(), 4 * 2 * 2, "2 bytes a pixel");
    // A 2×6 view of the same bytes: one pair a row, every row of it.
    let narrow = reference(&frame, 2, 6);
    assert_eq!(narrow.len(), 6 * 4);
    assert_eq!(narrow[..4], [171, 63, 179, 32]);
    assert_eq!(narrow[4..8], [128, 235, 128, 16]);
    assert_eq!(narrow[20..], pack_pair(bgra(7, 8, 9), bgra(10, 11, 12)));
}

#[test]
fn a_short_or_empty_frame_gives_the_rows_it_has() {
    let frame: Vec<u8> = [bgra(255, 0, 0), bgra(0, 0, 255)].concat();
    assert_eq!(
        reference(&frame, 2, 3),
        [171, 63, 179, 32],
        "one row of three"
    );
    assert!(reference(&[], 2, 3).is_empty());
    assert!(reference(&frame, 0, 2).is_empty(), "no pixel to pack");
}

/// An odd width's last pixel belongs to no pair: it is left out.
#[test]
fn an_odd_last_column_is_left_out() {
    let frame: Vec<u8> = [bgra(255, 0, 0), bgra(0, 0, 255), bgra(9, 9, 9)].concat();
    assert_eq!(reference(&frame, 3, 1), [171, 63, 179, 32]);
}

#[test]
fn the_uyvy_texture_is_half_the_width_and_the_full_height() {
    assert_eq!(uyvy_size(1920, 1080), Ok((960, 1080)));
    assert_eq!(uyvy_size(3840, 2160), Ok((1920, 2160)));
    assert_eq!(uyvy_size(2, 7), Ok((1, 7)));
}

#[test]
fn an_odd_width_is_refused() {
    assert_eq!(
        uyvy_size(1919, 1080),
        Err(GpuError::UyvyOddWidth { width: 1919 })
    );
    let error = uyvy_size(1, 1).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the UYVY pass needs a target of even width, this one is 1 pixels wide"
    );
    assert!(!error.is_device_lost());
}

/// A `DO_NOT_WAIT` map that finds the copy still drawing is an error that
/// names it, never a wait; a lost device stays a lost device; anything
/// else is the map's ordinary failure.
#[test]
fn a_failed_map_names_what_it_met() {
    let still = map_failed(DXGI_ERROR_WAS_STILL_DRAWING);
    assert_eq!(
        still,
        GpuError::Api {
            call: STILL_DRAWING_CALL,
            hresult: 0x887A_000A,
        }
    );
    assert!(still.to_string().contains("DXGI_ERROR_WAS_STILL_DRAWING"));
    assert!(!still.is_device_lost());
    assert_eq!(
        map_failed(DXGI_ERROR_DEVICE_REMOVED),
        GpuError::DeviceLost {
            call: MAP_CALL,
            hresult: DXGI_ERROR_DEVICE_REMOVED,
        }
    );
    assert_eq!(
        map_failed(0x8007_0057),
        GpuError::Api {
            call: MAP_CALL,
            hresult: 0x8007_0057,
        }
    );
}

#[test]
fn the_tolerance_is_one_code_and_the_ring_three_copies() {
    assert_eq!(UYVY_TOLERANCE, 1);
    assert_eq!(UYVY_STAGING, 3);
}

#[test]
fn a_ring_with_no_finished_copy_has_nothing_to_read() {
    let ring = StagingRing::new();
    assert_eq!(ring.newest(), None);
    assert_eq!(ring.slot_to_write(), 0);
    assert_eq!(ring, StagingRing::default());
}

/// A read changes nothing: the newest copy stays readable until the next
/// compose finishes, which writes the slot after it.
#[test]
fn a_write_after_a_read_moves_to_the_next_slot() {
    let mut ring = StagingRing::new();
    ring.written();
    assert_eq!(ring.newest(), Some(0));
    assert_eq!(ring.slot_to_write(), 1);
    assert_eq!(ring.newest(), Some(0), "read twice, still slot 0");
    ring.written();
    assert_eq!(ring.newest(), Some(1));
    assert_eq!(ring.slot_to_write(), 2);
}

/// Ten finished composes: the slots go 0, 1, 2, 0, …, and the slot a
/// compose copies into is never the one a readback maps.
#[test]
fn the_ring_wraps_and_never_writes_the_newest_copy() {
    let mut ring = StagingRing::new();
    let mut newest = Vec::new();
    for _ in 0..10 {
        let slot = ring.slot_to_write();
        assert_ne!(Some(slot), ring.newest(), "copy into the newest slot");
        ring.written();
        assert_eq!(ring.newest(), Some(slot));
        newest.push(slot);
    }
    assert_eq!(newest, [0, 1, 2, 0, 1, 2, 0, 1, 2, 0]);
    assert_eq!(ring.slot_to_write(), 1);
}
