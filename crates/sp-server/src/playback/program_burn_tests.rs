//! #228: the `SP-program` burn — #151's payload and paint, its new place,
//! and a round trip through rqrr (camera-box's decoder). Wired via
//! `#[cfg(test)] #[path = "program_burn_tests.rs"] mod tests;` in
//! `program_burn.rs`.

use qrcode::{Color, EcLevel, QrCode, Version};

use super::*;
use crate::playback::program_transition::Layout;

const W: u32 = 1920;
const H: u32 = 1080;

/// A `w`×`h` canvas layout (stride `w`, whole NV12).
fn canvas(w: u32, h: u32) -> Layout {
    let luma = w as usize * h as usize;
    Layout {
        width: w,
        height: h,
        stride: w,
        len: luma + luma / 2,
    }
}

/// A `w`×`h` NV12 picture of luma `y`, chroma `uv`.
fn picture(w: u32, h: u32, y: u8, uv: u8) -> Vec<u8> {
    let luma = w as usize * h as usize;
    let mut buf = vec![y; luma];
    buf.resize(luma + luma / 2, uv);
    buf
}

/// A rectangle `[x, x + w) × [y, y + h)`.
#[derive(Clone, Copy, Debug)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Rect {
    fn overlaps(self, other: Rect) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }

    /// Grown by `pad` on every side.
    fn padded(self, pad: u32) -> Rect {
        Rect {
            x: self.x - pad,
            y: self.y - pad,
            w: self.w + 2 * pad,
            h: self.h + 2 * pad,
        }
    }
}

/// The burn's whole square on 1920×1080.
fn burn_square() -> Rect {
    let g = geometry(W, H).unwrap();
    Rect {
        x: g.x,
        y: g.y,
        w: g.side,
        h: g.side,
    }
}

/// The burn's dark modules' area (inside its own quiet zone) on 1920×1080.
fn burn_code() -> Rect {
    let g = geometry(W, H).unwrap();
    let quiet = QUIET_MODULES * g.module;
    Rect {
        x: g.x + quiet,
        y: g.y + quiet,
        w: QR_MODULES * g.module,
        h: QR_MODULES * g.module,
    }
}

// ---- CRC-32 and the payload (#151's vectors, camera-box `payload.rs`) ----

#[test]
fn crc32_is_iso_hdlc() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(crc32(b""), 0);
}

#[test]
fn the_payload_is_151_s_wire_format() {
    assert_eq!(
        payload(42, 9001, 1_234_567_890),
        "P42.9001.1234567890.1303268857"
    );
    // #151's committed fixture vector (for camera-box#1301).
    let fixture = include_str!("../../../../eval/fixtures/burn/sp-burn-1080p.txt").trim();
    assert_eq!(fixture, "P911014.1.1700000000000000000.1242144");
    assert_eq!(
        payload(SONGPLAYER_RUN_ID, 1, 1_700_000_000_000_000_000),
        fixture
    );
}

// ---- The place ----

#[test]
fn the_burn_sits_top_right_four_px_a_module_on_the_program_canvas() {
    assert_eq!(BURN_MODULES, QR_MODULES + 2 * QUIET_MODULES);
    assert_eq!(
        geometry(W, H),
        Some(BurnGeom {
            module: 4,
            side: 148,
            x: 1760,
            y: 12,
        })
    );
    // It scales with the height: 8 px a module on 2160 rows.
    assert_eq!(
        geometry(3840, 2160),
        Some(BurnGeom {
            module: 8,
            side: 296,
            x: 3520,
            y: 24,
        })
    );
}

#[test]
fn a_canvas_too_small_for_the_square_has_no_burn() {
    assert_eq!(geometry(W, 269), None, "under one px a module");
    assert_eq!(geometry(159, H), None, "narrower than the square + margin");
    assert_eq!(
        geometry(160, H).map(|g| g.x),
        Some(0),
        "exactly the square + its margin"
    );
}

/// camera-box `src/burn_regions.rs::slot_rect` on 1920×1080 (its own test
/// `production_1080_slots_match_the_documented_burn_positions`), each grown
/// by its 8 px recovery pad: the burn overlaps none of them.
#[test]
fn the_burn_overlaps_no_camera_box_node_burn_slot() {
    let slots = [
        ("CameraCapture", 800, 736, 320),
        ("BottomLeft", 40, 738, 302),
        ("BottomCenterLeft", 382, 738, 302),
        ("BottomCenterRight", 1236, 738, 302),
        ("BottomRight", 1578, 738, 302),
    ];
    for (name, x, y, side) in slots {
        let slot = Rect {
            x,
            y,
            w: side,
            h: side,
        }
        .padded(8);
        assert!(!burn_square().overlaps(slot), "{name}: {slot:?}");
    }
}

/// camera-box's measurement clip (frame 1800 of the delivered 128 s file):
/// two 29-module QRs at 18 px a module, their images with the 4-module quiet
/// zone at `[147, 813)` and `[1107, 1773)` × `[24, 690)`, the frame counter
/// at `[884, 1074)` × `[330, 418)`. The burn's dark modules stay out of both
/// QR images, its whole square out of their dark modules and the counter.
#[test]
fn the_burn_keeps_clear_of_the_measurement_clip_s_qrs() {
    let images = [
        Rect {
            x: 147,
            y: 24,
            w: 666,
            h: 666,
        },
        Rect {
            x: 1107,
            y: 24,
            w: 666,
            h: 666,
        },
    ];
    let codes = [
        Rect {
            x: 219,
            y: 96,
            w: 522,
            h: 522,
        },
        Rect {
            x: 1179,
            y: 96,
            w: 522,
            h: 522,
        },
    ];
    let counter = Rect {
        x: 884,
        y: 330,
        w: 190,
        h: 88,
    };
    for image in images {
        assert!(!burn_code().overlaps(image), "{image:?}");
    }
    for code in codes {
        assert!(!burn_square().overlaps(code), "{code:?}");
    }
    assert!(!burn_square().overlaps(counter));
    assert_eq!(
        burn_code().x,
        1776,
        "right of the clip's right QR image (1773)"
    );
}

// ---- The QR ----

#[test]
fn the_qr_is_a_version_3_code_inside_a_light_quiet_zone() {
    let modules = qr_modules("P911014.1.1700000000000000000.1242144").unwrap();
    let n = BURN_MODULES as usize;
    assert_eq!(modules.len(), n * n);
    let quiet = QUIET_MODULES as usize;
    for i in 0..n {
        for q in 0..quiet {
            assert!(!modules[q * n + i], "top quiet zone, row {q}");
            assert!(!modules[(n - 1 - q) * n + i], "bottom quiet zone");
            assert!(!modules[i * n + q], "left quiet zone");
            assert!(!modules[i * n + n - 1 - q], "right quiet zone");
        }
    }
    // The top-left finder: its outer ring and its 3×3 core dark, the ring
    // between them light.
    let at = |row: usize, column: usize| modules[(row + quiet) * n + column + quiet];
    assert!(at(0, 0) && at(0, 6) && at(6, 0) && at(6, 6));
    assert!(!at(1, 1) && !at(5, 5));
    assert!(at(2, 2) && at(3, 3) && at(4, 4));
    // The grid is the code's own modules, shifted by the quiet zone.
    let code = QrCode::with_version(
        b"P911014.1.1700000000000000000.1242144",
        Version::Normal(3),
        EcLevel::M,
    )
    .unwrap();
    assert_eq!(code.width(), QR_MODULES as usize);
    let colors = code.into_colors();
    let w = QR_MODULES as usize;
    for row in 0..w {
        for column in 0..w {
            let dark = colors[row * w + column] == Color::Dark;
            assert_eq!(at(row, column), dark, "module ({row}, {column})");
        }
    }
}

#[test]
fn the_longest_payload_still_fits_the_fixed_qr() {
    let longest = payload(SONGPLAYER_RUN_ID, u32::MAX, i64::MAX);
    assert_eq!(longest, "P911014.4294967295.9223372036854775807.515822610");
    assert!(qr_modules(&longest).is_some());
    assert!(
        qr_modules(&"9".repeat(400)).is_none(),
        "far over a version 3 QR"
    );
}

// ---- The paint ----

/// A checkerboard of modules, so every module's place is visible.
fn checkerboard() -> Vec<bool> {
    let n = BURN_MODULES as usize;
    (0..n * n).map(|i| (i / n + i % n) % 2 == 0).collect()
}

/// What `paint` must leave: `before` with the square's luma set by module
/// and its chroma neutral — written out by hand, pixel by pixel.
fn expected_paint(before: &[u8], modules: &[bool]) -> Vec<u8> {
    let mut out = before.to_vec();
    let (w, n) = (W as usize, BURN_MODULES as usize);
    for ry in 0..148usize {
        for rx in 0..148usize {
            let dark = modules[(ry / 4) * n + rx / 4];
            out[(12 + ry) * w + 1760 + rx] = if dark { 16 } else { 235 };
        }
    }
    let luma = w * H as usize;
    for cy in 6..80usize {
        let row = luma + cy * w;
        out[row + 1760..row + 1908].fill(128);
    }
    out
}

#[test]
fn the_paint_sets_the_square_by_module_and_touches_nothing_else() {
    let before = picture(W, H, 100, 50);
    let modules = checkerboard();
    let mut buf = before.clone();
    paint(&mut buf, canvas(W, H), &modules);
    assert!(buf == expected_paint(&before, &modules));
    // Spot checks: the square's corners, its outside neighbours.
    let at = |x: usize, y: usize| buf[y * W as usize + x];
    assert_eq!(
        at(1760, 12),
        16,
        "module (0, 0) of the checkerboard is dark"
    );
    assert_eq!(at(1764, 12), 235, "module (0, 1) is light");
    assert_eq!(at(1759, 12), 100, "left of the square");
    assert_eq!(at(1908, 12), 100, "right of the square");
    assert_eq!(at(1760, 11), 100, "above the square");
    assert_eq!(at(1760, 160), 100, "below the square");
}

#[test]
fn the_paint_writes_nothing_it_cannot_place() {
    let modules = checkerboard();
    // A grid of the wrong size.
    let before = picture(W, H, 100, 50);
    let mut buf = before.clone();
    paint(&mut buf, canvas(W, H), &modules[1..]);
    assert!(buf == before, "a grid that is not 37 × 37");
    // A stride narrower than the picture.
    let narrow = Layout {
        stride: 1000,
        ..canvas(W, H)
    };
    paint(&mut buf, narrow, &modules);
    assert!(buf == before, "stride under the width");
    // A buffer short of whole NV12 (its luma plane only).
    let luma_only = W as usize * H as usize;
    let mut short = vec![100u8; luma_only];
    paint(&mut short, canvas(W, H), &modules);
    assert!(short.iter().all(|&b| b == 100), "not whole NV12");
    // A canvas with no room for the square.
    let small = picture(W, 200, 100, 50);
    let mut buf = small.clone();
    paint(&mut buf, canvas(W, 200), &modules);
    assert!(buf == small, "under 270 rows");
}

#[test]
fn the_paint_of_an_exactly_whole_picture_paints() {
    // The canvas picture is exactly w·h·3/2 bytes: the guard lets it through.
    let before = picture(W, H, 100, 50);
    assert_eq!(before.len(), canvas(W, H).len);
    let mut buf = before.clone();
    paint(&mut buf, canvas(W, H), &checkerboard());
    assert_ne!(buf, before);
}

#[test]
fn burned_paints_a_copy_and_leaves_the_picture_as_it_was() {
    let before = picture(W, H, 100, 50);
    let video = before.clone();
    let out = burned(&video, canvas(W, H), 1800, 1_760_000_000_000_000_000).unwrap();
    assert!(
        video == before,
        "the canvas picture itself is never written"
    );
    let modules = qr_modules(&payload(SONGPLAYER_RUN_ID, 1800, 1_760_000_000_000_000_000));
    assert!(out[..] == expected_paint(&before, &modules.unwrap())[..]);
    assert!(burned(&video, canvas(W, 200), 1800, 0).is_none(), "no room");
}

// ---- A round trip through rqrr (camera-box's decoder) ----

/// Every QR rqrr reads in the luma rectangle `[x, x + w) × [y, y + h)`.
fn decode(luma: &[u8], stride: usize, rect: Rect) -> Vec<String> {
    let (x0, y0) = (rect.x as usize, rect.y as usize);
    let mut image =
        rqrr::PreparedImage::prepare_from_greyscale(rect.w as usize, rect.h as usize, |x, y| {
            luma[(y0 + y) * stride + x0 + x]
        });
    image
        .detect_grids()
        .into_iter()
        .filter_map(|grid| grid.decode().ok().map(|(_, text)| text))
        .collect()
}

/// The top-right corner around the burn.
const CORNER: Rect = Rect {
    x: 1720,
    y: 0,
    w: 200,
    h: 200,
};

#[test]
fn the_burn_decodes_to_its_payload_on_white_grey_and_black() {
    let expected = payload(SONGPLAYER_RUN_ID, 1800, 1_760_000_000_000_000_000);
    for background in [255u8, 128, 16] {
        let video = picture(W, H, background, 128);
        let out = burned(&video, canvas(W, H), 1800, 1_760_000_000_000_000_000).unwrap();
        assert_eq!(
            decode(&out, W as usize, CORNER),
            vec![expected.clone()],
            "background luma {background}"
        );
    }
    // The whole picture (white), as a full-frame pass reads it.
    let out = burned(&picture(W, H, 255, 128), canvas(W, H), 7, 42).unwrap();
    let whole = Rect {
        x: 0,
        y: 0,
        w: W,
        h: H,
    };
    assert_eq!(
        decode(&out, W as usize, whole),
        vec![payload(SONGPLAYER_RUN_ID, 7, 42)]
    );
}

/// Draw a QR of `text` (version 3, level H, the clip's) with 18 px modules
/// and a 4-module quiet zone, its image's top-left corner at `(x, y)`.
fn draw_clip_qr(luma: &mut [u8], text: &str, x: usize, y: usize) {
    let code = QrCode::with_version(text.as_bytes(), Version::Normal(3), EcLevel::H).unwrap();
    let n = code.width();
    let colors = code.into_colors();
    for my in 0..n {
        for mx in 0..n {
            if colors[my * n + mx] == Color::Dark {
                for py in 0..18 {
                    let row = (y + 72 + my * 18 + py) * W as usize;
                    luma[row + x + 72 + mx * 18..][..18].fill(0);
                }
            }
        }
    }
}

/// The clip's picture with the burn on it: its two QRs still decode from
/// their own halves of the top band, and the burn from the corner.
#[test]
fn the_clip_s_qrs_and_the_burn_decode_side_by_side() {
    let left = "P911016.3600.60000000000.1";
    let right = "P911016.3599.59983333333.2";
    let mut clip = picture(W, H, 255, 128);
    draw_clip_qr(&mut clip, left, 147, 24);
    draw_clip_qr(&mut clip, right, 1107, 24);
    let out = burned(&clip, canvas(W, H), 1800, 1_760_000_000_000_000_000).unwrap();
    let half = |x: u32| Rect {
        x,
        y: 0,
        w: 960,
        h: 700,
    };
    assert_eq!(decode(&out, W as usize, half(0)), vec![left.to_string()]);
    assert!(
        decode(&out, W as usize, half(960)).contains(&right.to_string()),
        "the clip's right QR still decodes next to the burn"
    );
    assert_eq!(
        decode(&out, W as usize, CORNER),
        vec![payload(SONGPLAYER_RUN_ID, 1800, 1_760_000_000_000_000_000)]
    );
}

/// The installer ships THIRD-PARTY-NOTICES.txt; it must carry the MIT notice
/// of the qrcode this crate pins (re-copy it when the pin moves).
#[test]
fn the_installer_notice_carries_the_pinned_qrcodes_license() {
    const NOTICE: &str = include_str!("../../../../src-tauri/resources/THIRD-PARTY-NOTICES.txt");
    const MANIFEST: &str = include_str!("../../Cargo.toml");
    let notice = NOTICE.replace("\r\n", "\n");
    assert!(
        MANIFEST.contains("qrcode = { version = \"=0.14.1\""),
        "the pin"
    );
    assert!(
        notice.contains("qrcode 0.14.1"),
        "the notice names the pinned version"
    );
    assert!(notice.contains("Copyright (c) 2016 kennytm"));
}
