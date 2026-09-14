//! NV12 burn-id QR overlay (#151) — the paced-path compositor.
//!
//! Renders the fleet burn-id (`sp_core::genlock::burn`) as a QR into the LUMA
//! plane of an NV12 frame, bottom-right, luma-only (black 16 / white 235 in
//! limited range, matching the fleet decoder's input), chroma left neutral (128)
//! over the burn rectangle. Only the `genlock_pacing` paced emit path
//! (`FrameSubmitter::submit_frame_at_boundary`) calls this — never the legacy
//! SDK-clocked `decode_and_send` path (a QR must never reach a non-genlock
//! output). Default OFF, toggled at runtime via `POST /api/v1/ndi/burn`, never
//! persisted.

use qrcode::{Color, EcLevel, QrCode};
use sp_core::genlock::burn::BurnGeom;

/// Limited-range luma for a dark QR module (the fleet decoder thresholds this).
const LUMA_DARK: u8 = 16;
/// Limited-range luma for a light QR module / quiet border.
const LUMA_LIGHT: u8 = 235;
/// Neutral chroma (no colour) written over the burn rectangle's UV samples.
const CHROMA_NEUTRAL: u8 = 128;
/// Quiet-zone width in modules around the QR (the QR-spec minimum is 4).
const QUIET_ZONE: usize = 4;

/// Encode `payload` as a QR (EC level M, smallest fitting version) and return
/// `(module_side, modules)` — a `module_side × module_side` row-major grid
/// (`true` = dark) that INCLUDES a 4-module quiet-zone border. `None` if the
/// payload cannot be encoded (never in practice — the burn-id is ~40 bytes).
pub fn render_qr_modules(payload: &str) -> Option<(usize, Vec<bool>)> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M).ok()?;
    let w = code.width();
    let colors = code.into_colors();
    let side = w + 2 * QUIET_ZONE;
    let mut modules = vec![false; side * side];
    for y in 0..w {
        for x in 0..w {
            if colors[y * w + x] == Color::Dark {
                modules[(y + QUIET_ZONE) * side + (x + QUIET_ZONE)] = true;
            }
        }
    }
    Some((side, modules))
}

/// Paint the QR `modules` (a `module_side × module_side` grid) into the NV12
/// buffer `nv12` at `geom` (the bottom-right rectangle). Luma-only: every luma
/// pixel inside the `geom.side × geom.side` rectangle becomes [`LUMA_DARK`]
/// (dark module) or [`LUMA_LIGHT`] (light module / quiet border); the covering
/// chroma samples are set to [`CHROMA_NEUTRAL`]. Pixels OUTSIDE the rectangle
/// are never touched. Pure — unit-tested on synthetic buffers.
///
/// The QR is scaled to the rectangle by an INTEGER module scale (no
/// interpolation, so modules stay crisp) and centred; any leftover border is
/// filled light (extra quiet zone). No-op if the rectangle is too small for even
/// a 1× QR (never at 302 px / 33 modules).
pub fn paint_nv12(
    nv12: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    geom: BurnGeom,
    module_side: usize,
    modules: &[bool],
) {
    if module_side == 0 || modules.len() < module_side * module_side {
        return;
    }
    let stride = stride as usize;
    let width = width as usize;
    let height = height as usize;
    let side = geom.side as usize;
    let x0 = geom.x as usize;
    let y0 = geom.y as usize;

    let y_len = stride * height;
    if nv12.len() < y_len {
        return;
    }
    let scale = side / module_side;
    if scale == 0 {
        return;
    }
    let rendered = scale * module_side;
    let pad = (side - rendered) / 2;

    let (y_plane, uv_plane) = nv12.split_at_mut(y_len);

    // --- Luma plane ---
    for ry in 0..side {
        let py = y0 + ry;
        if py >= height {
            break;
        }
        let row_base = py * stride;
        for rx in 0..side {
            let px = x0 + rx;
            if px >= width {
                break;
            }
            let val = if ry < pad || ry >= pad + rendered || rx < pad || rx >= pad + rendered {
                LUMA_LIGHT
            } else {
                let mx = (rx - pad) / scale;
                let my = (ry - pad) / scale;
                if modules[my * module_side + mx] {
                    LUMA_DARK
                } else {
                    LUMA_LIGHT
                }
            };
            y_plane[row_base + px] = val;
        }
    }

    // --- Chroma plane (NV12: interleaved U,V, one pair per 2×2 luma block) ---
    // A luma pixel (px, py) maps to chroma sample (px/2, py/2), stored at
    // uv_plane[(py/2)*stride + (px/2)*2 ..+2].
    let cx_start = x0 / 2;
    let cx_end = (x0 + side).div_ceil(2);
    let cy_start = y0 / 2;
    let cy_end = (y0 + side).div_ceil(2);
    let chroma_rows = uv_plane.len() / stride;
    for cy in cy_start..cy_end.min(chroma_rows) {
        let row_base = cy * stride;
        for cx in cx_start..cx_end {
            let u = row_base + cx * 2;
            if u + 1 >= uv_plane.len() {
                break;
            }
            uv_plane[u] = CHROMA_NEUTRAL;
            uv_plane[u + 1] = CHROMA_NEUTRAL;
        }
    }
}

/// Render + composite the burn-id for one paced frame into `nv12` (a buffer the
/// caller OWNS — the pacer's per-frame `to_vec` clone, never the decoder's
/// memory). `frame_id` is the pacing `seq`; `gen_ts_ns` is the serviced boundary
/// wall time in nanoseconds. No-op if the QR cannot be encoded.
pub fn paint_burn(
    nv12: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    frame_id: u32,
    gen_ts_ns: i64,
) {
    let geom = sp_core::genlock::burn::geometry(width, height);
    let payload = sp_core::genlock::burn::payload(
        sp_core::genlock::burn::SONGPLAYER_RUN_ID,
        frame_id,
        gen_ts_ns,
    );
    if let Some((module_side, modules)) = render_qr_modules(&payload) {
        paint_nv12(nv12, width, height, stride, geom, module_side, &modules);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_qr_modules_is_square_with_light_quiet_border() {
        let (side, modules) = render_qr_modules("P911014.1.1700000000000000000.1242144").unwrap();
        assert_eq!(modules.len(), side * side);
        assert!(side >= 8 + 21, "at least version-1 (21) + 8 quiet modules");
        // The outer 4-module quiet zone is all light — check the outermost ring.
        for i in 0..side {
            assert!(!modules[i], "top row must be light quiet zone");
            assert!(!modules[(side - 1) * side + i], "bottom row must be light");
            assert!(!modules[i * side], "left col must be light");
            assert!(!modules[i * side + side - 1], "right col must be light");
        }
        // A real QR has finder patterns => some dark modules.
        assert!(modules.iter().any(|&m| m), "QR must have dark modules");
    }

    #[test]
    fn paint_nv12_writes_only_the_rectangle_with_luma_16_or_235() {
        // 16×16 NV12 with a manual geometry + a 5×5 checkerboard module grid so
        // the exact painted region is deterministic and independent of `qrcode`.
        let (w, h, stride) = (16u32, 16u32, 16u32);
        let y_len = (stride * h) as usize;
        let uv_len = (stride * (h / 2)) as usize;
        let sentinel_y = 100u8;
        let sentinel_uv = 50u8;
        let mut buf = vec![sentinel_y; y_len + uv_len];
        buf[y_len..].fill(sentinel_uv);

        let geom = BurnGeom {
            side: 10,
            margin: 2,
            x: 4,
            y: 4,
        };
        let module_side = 5usize;
        let modules: Vec<bool> = (0..module_side * module_side)
            .map(|i| ((i / module_side) + (i % module_side)) % 2 == 0)
            .collect();

        paint_nv12(&mut buf, w, h, stride, geom, module_side, &modules);

        let (y_plane, uv_plane) = buf.split_at(y_len);
        let stride = stride as usize;

        let mut saw_dark = false;
        let mut saw_light = false;
        for py in 0..h as usize {
            for px in 0..stride {
                let v = y_plane[py * stride + px];
                let inside = (4..14).contains(&px) && (4..14).contains(&py);
                if inside {
                    assert!(
                        v == 16 || v == 235,
                        "luma inside rect must be 16/235, got {v} at ({px},{py})"
                    );
                    saw_dark |= v == 16;
                    saw_light |= v == 235;
                } else {
                    assert_eq!(
                        v, sentinel_y,
                        "luma OUTSIDE rect must be untouched at ({px},{py})"
                    );
                }
            }
        }
        assert!(
            saw_dark && saw_light,
            "the checkerboard must paint BOTH dark(16) and light(235), not a flat fill"
        );

        // Chroma: neutral 128 over the covering region [2,7)×[2,7), untouched else.
        let chroma_rows = h as usize / 2;
        for cy in 0..chroma_rows {
            for cx in 0..stride / 2 {
                let base = cy * stride + cx * 2;
                let u = uv_plane[base];
                let v = uv_plane[base + 1];
                let inside = (2..7).contains(&cx) && (2..7).contains(&cy);
                if inside {
                    assert_eq!(u, 128, "U neutral inside at ({cx},{cy})");
                    assert_eq!(v, 128, "V neutral inside at ({cx},{cy})");
                } else {
                    assert_eq!(u, sentinel_uv, "U untouched outside at ({cx},{cy})");
                    assert_eq!(v, sentinel_uv, "V untouched outside at ({cx},{cy})");
                }
            }
        }
    }

    #[test]
    fn paint_nv12_is_noop_when_rectangle_smaller_than_qr() {
        // side 3 < module_side 5 -> scale 0 -> skip, buffer untouched.
        let mut buf = vec![7u8; 16 * 16 * 3 / 2];
        let before = buf.clone();
        let geom = BurnGeom {
            side: 3,
            margin: 2,
            x: 4,
            y: 4,
        };
        paint_nv12(&mut buf, 16, 16, 16, geom, 5, &vec![true; 25]);
        assert_eq!(buf, before, "too-small rectangle must not paint");
    }

    #[test]
    fn paint_burn_paints_the_1080p_bottom_right_and_nothing_above() {
        // Full-path smoke on a real 1080p NV12: paint_burn must touch only the
        // bottom-right burn rect, leaving the top of the frame pristine.
        let (w, h, stride) = (1920u32, 1080u32, 1920u32);
        let y_len = (stride * h) as usize;
        let uv_len = (stride * (h / 2)) as usize;
        let mut buf = vec![100u8; y_len + uv_len];

        paint_burn(&mut buf, w, h, stride, 7, 1_700_000_000_000_000_000);

        let geom = sp_core::genlock::burn::geometry(w, h);
        // Top-left pixel is far above the rect -> untouched.
        assert_eq!(buf[0], 100, "top-left luma must be untouched");
        // A pixel inside the burn rect -> painted to a QR luma value.
        let cx = geom.x as usize + geom.side as usize / 2;
        let cy = geom.y as usize + geom.side as usize / 2;
        let v = buf[cy * stride as usize + cx];
        assert!(
            v == 16 || v == 235,
            "burn-rect luma must be 16/235, got {v}"
        );
    }
}

#[cfg(test)]
#[path = "burn_overlay_tests_mutants.rs"]
mod burn_overlay_tests_mutants;
