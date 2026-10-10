//! The burn-id QR of `SP-program` (#228): SongPlayer's fleet origin burn
//! 911014, painted onto the program's canvas picture before its NDI submit.
//!
//! The payload is #151's, byte for byte: `P{run}.{frame}.{gen_ts_ns}.{crc}`,
//! the CRC-32 (ISO-HDLC) of the dotted body — camera-box's
//! `probe::payload::Payload::encode`. `{frame}` is the on-air item's frame
//! index from its frame 0 (`program_item.rs`), `{gen_ts_ns}` the boundary's
//! wire stamp in nanoseconds (what the NDI timecode carries).
//!
//! The place is NOT #151's. #151 burned bottom-right, `(1578, 738, 302,
//! 302)` on 1920×1080: that IS camera-box's `BurnSlot::BottomRight` (the
//! stream's own burn), and camera-box's five node-burn slots fill the bottom
//! band edge to edge. camera-box's measurement clip holds two QR images
//! (with their quiet zones `[147, 813)` and `[1107, 1773)` × `[24, 690)`) and
//! a frame counter in the top band. So the burn sits TOP-RIGHT, small enough
//! to stay right of the clip's right QR image (design record on #228):
//!
//! - a version 3 QR at error-correction level M (every payload of this
//!   format fits: at most 49 alphanumeric characters), 29 modules, plus a
//!   4-module quiet zone = [`BURN_MODULES`] = 37 modules a side;
//! - one module = `height / 270` px (4 px on 1080), the square
//!   [`BURN_MODULES`] modules, [`MARGIN_MODULES`] modules from the top and
//!   the right edge: `(1760, 12, 148, 148)` on 1920×1080, its dark modules
//!   at x ≥ 1776.
//!
//! The paint is #151's: luma 16 for a dark module, 235 for a light one and
//! the quiet zone, neutral chroma 128 over the square, nothing outside it.
//! [`burned`] paints into a COPY: the canvas picture can be the source's own
//! allocation, which `SP-program-MAX` and the Spout FHD sender also hold —
//! they never carry the burn.

use qrcode::{Color, EcLevel, QrCode, Version};

use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_transition::Layout;

/// SongPlayer's reserved fleet run id (camera-box#1301).
pub const SONGPLAYER_RUN_ID: u32 = 911_014;

/// The QR version every burn uses (29 modules): a fixed size, so the burn's
/// square never changes from one frame to the next.
const QR_VERSION: i16 = 3;

/// Modules of the QR itself (version 3: 17 + 4 · 3).
pub const QR_MODULES: u32 = 29;

/// The quiet zone around the QR, in modules (the QR spec's minimum).
pub const QUIET_MODULES: u32 = 4;

/// The burn's square, in modules: the QR and its quiet zone on both sides.
pub const BURN_MODULES: u32 = 37;

/// The square's distance from the top and the right edge, in modules.
pub const MARGIN_MODULES: u32 = 3;

/// Canvas rows per module px: 4 px a module on 1080 rows.
pub const ROWS_PER_MODULE_PX: u32 = 270;

/// Limited-range luma of a dark module (the fleet decoder thresholds this).
const LUMA_DARK: u8 = 16;
/// Limited-range luma of a light module and of the quiet zone.
const LUMA_LIGHT: u8 = 235;
/// Neutral chroma written over the square.
const CHROMA_NEUTRAL: u8 = 128;

/// Where the burn sits on a canvas: its square's top-left corner and side,
/// and one module's size, in px.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BurnGeom {
    pub module: u32,
    pub side: u32,
    pub x: u32,
    pub y: u32,
}

/// The burn's square on a `width`×`height` canvas: top-right, one module =
/// `height / 270` px, [`MARGIN_MODULES`] modules from the top and the right
/// edge. `None` for a canvas under 270 rows or too narrow for the square and
/// its margin.
pub fn geometry(width: u32, height: u32) -> Option<BurnGeom> {
    let module = height / ROWS_PER_MODULE_PX;
    if module == 0 {
        return None;
    }
    let side = BURN_MODULES * module;
    let margin = MARGIN_MODULES * module;
    let x = width.checked_sub(margin + side)?;
    Some(BurnGeom {
        module,
        side,
        x,
        y: margin,
    })
}

/// CRC-32/ISO-HDLC (reflected polynomial `0xEDB88320`, init and xorout all
/// ones), the one camera-box's payload uses (`crc` crate `CRC_32_ISO_HDLC`;
/// check value of `"123456789"` = `0xCBF43926`). The table is built at
/// compile time.
const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// The CRC-32 of `data` (see [`CRC32_TABLE`]).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let index = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[index];
    }
    !crc
}

/// The burn-id wire string `P{run_id}.{frame}.{gen_ts_ns}.{crc}`, the CRC
/// covering the dotted body `run_id.frame.gen_ts_ns` — camera-box
/// `Payload::encode`, and #151's `genlock::burn::payload`.
pub fn payload(run_id: u32, frame: u32, gen_ts_ns: i64) -> String {
    let body = format!("{run_id}.{frame}.{gen_ts_ns}");
    let crc = crc32(body.as_bytes());
    format!("P{body}.{crc}")
}

/// The QR of `payload` (version 3, level M) with its quiet zone, as a
/// [`BURN_MODULES`]² row-major grid, `true` = dark. `None` when the payload
/// does not fit a version 3 QR (never for this payload format).
pub fn qr_modules(payload: &str) -> Option<Vec<bool>> {
    let code =
        QrCode::with_version(payload.as_bytes(), Version::Normal(QR_VERSION), EcLevel::M).ok()?;
    let width = code.width();
    let colors = code.into_colors();
    let side = BURN_MODULES as usize;
    let quiet = QUIET_MODULES as usize;
    let mut modules = vec![false; side * side];
    for (index, color) in colors.iter().enumerate() {
        if *color == Color::Dark {
            let (row, column) = (index / width, index % width);
            modules[(row + quiet) * side + column + quiet] = true;
        }
    }
    Some(modules)
}

/// Paint the [`BURN_MODULES`]² grid `modules` into the NV12 picture `nv12`
/// of `layout`, at [`geometry`]: each luma pixel of the square is
/// [`LUMA_DARK`] or [`LUMA_LIGHT`] by its module, the chroma samples over
/// the square are neutral, nothing else is touched. Paints nothing when the
/// canvas has no room for the square, the grid is not that size, or the
/// picture is not whole NV12 for its layout.
pub fn paint(nv12: &mut [u8], layout: Layout, modules: &[bool]) {
    let Some(geom) = geometry(layout.width, layout.height) else {
        return;
    };
    let n = BURN_MODULES as usize;
    let stride = layout.stride as usize;
    let luma = stride * layout.height as usize;
    if modules.len() != n * n || layout.stride < layout.width || nv12.len() < luma + luma / 2 {
        return;
    }
    let (module, side) = (geom.module as usize, geom.side as usize);
    let (x0, y0) = (geom.x as usize, geom.y as usize);
    let (y_plane, uv_plane) = nv12.split_at_mut(luma);
    for row in 0..side {
        let marks = &modules[row / module * n..][..n];
        let start = (y0 + row) * stride + x0;
        for (column, px) in y_plane[start..start + side].iter_mut().enumerate() {
            *px = if marks[column / module] {
                LUMA_DARK
            } else {
                LUMA_LIGHT
            };
        }
    }
    // A chroma sample (U, V) covers a 2×2 luma block: the samples under the
    // square, rows y0/2 .. ⌈(y0 + side)/2⌉, bytes x0/2·2 .. ⌈(x0 + side)/2⌉·2.
    let (first, last) = (x0 / 2 * 2, (x0 + side).div_ceil(2) * 2);
    for chroma_row in y0 / 2..(y0 + side).div_ceil(2) {
        let start = chroma_row * stride;
        uv_plane[start + first..start + last].fill(CHROMA_NEUTRAL);
    }
}

/// The canvas picture `video` (of `canvas`) with the burn of `frame` at
/// `gen_ts_ns` painted into a COPY taken from the frame pool: `video`
/// itself is never written. `None` when the canvas has no room for the burn
/// or the payload does not fit the QR.
pub fn burned(video: &[u8], canvas: Layout, frame: u32, gen_ts_ns: i64) -> Option<SharedFrame> {
    geometry(canvas.width, canvas.height)?;
    let modules = qr_modules(&payload(SONGPLAYER_RUN_ID, frame, gen_ts_ns))?;
    let mut out = sp_decoder::frame_pool::take(video.len());
    out.extend_from_slice(video);
    paint(&mut out, canvas, &modules);
    Some(SharedFrame::new(out))
}

#[cfg(test)]
#[path = "program_burn_tests.rs"]
mod tests;
