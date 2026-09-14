//! Genlock burn-id payload + QR corner geometry (#151).
//!
//! Pure and WASM-safe: no clock calls, no external crate. This is the SongPlayer
//! port of camera-box's `src/probe/payload.rs` (the `P{run}.{frame}.{ts}.{crc}`
//! wire format + its CRC-32) and `vendor/distroav/src/burn-geom.hpp` (the
//! bottom-corner QR placement), so the fleet's `recording-verdict` decodes
//! frames that ORIGINATE in SongPlayer exactly as it decodes camera-box's.
//!
//! The actual QR encoding + NV12 compositing live in `sp-server`
//! (`playback/burn_overlay.rs`, needs the `qrcode` crate) — this crate carries
//! only the pure, cross-platform numbers so they are unit-tested against
//! camera-box's own vectors in one place.

/// SongPlayer's reserved fleet run id (camera-box#1301).
pub const SONGPLAYER_RUN_ID: u32 = 911_014;

/// QR side as a fraction of the canvas HEIGHT — camera-box
/// `BURN_QR_HEIGHT_FRACTION = 0.28`, expressed as the integer ratio `28/100` so
/// the result is FP-free and matches the fleet's `burn_qr_px_for_canvas` numbers
/// exactly (302 px @1080, 604 px @2160 — see `tests/burn_payload_parity.rs`).
const QR_HEIGHT_NUM: u32 = 28;
const QR_HEIGHT_DEN: u32 = 100;
/// A tiny canvas still gets a readable burn (camera-box floors at 64 px).
const QR_MIN_PX: u32 = 64;

/// Edge margin as a fraction of the canvas HEIGHT — camera-box
/// `BURN_MARGIN_FRACTION = 40/1080`, integer ratio → 40 px @1080, 80 px @2160.
const MARGIN_NUM: u32 = 40;
const MARGIN_DEN: u32 = 1080;
/// Floor so a tiny canvas still has a quiet border (camera-box floors at 8 px).
const MARGIN_MIN_PX: u32 = 8;

/// Bottom-right QR burn placement for a canvas. `x`/`y` are the TOP-LEFT corner
/// of the `side`×`side` square; its bottom edge sits at `frame_h - margin` and
/// its right edge at `frame_w - margin` (camera-box `Corner::BottomRight`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurnGeom {
    /// QR square side, px.
    pub side: u32,
    /// Edge margin, px.
    pub margin: u32,
    /// Left edge of the square, px.
    pub x: u32,
    /// Top edge of the square, px.
    pub y: u32,
}

/// Compute the bottom-right burn geometry for a canvas — camera-box
/// `corner_placement(_, _, Corner::BottomRight, _, _)`. Integer math throughout
/// (deterministic, FP-free); `side`/`margin` reproduce the fleet's
/// canvas-relative numbers. `saturating_sub` keeps `x`/`y` in-frame on a
/// degenerate canvas smaller than `margin + side`.
pub fn geometry(frame_w: u32, frame_h: u32) -> BurnGeom {
    let side = (QR_HEIGHT_NUM * frame_h / QR_HEIGHT_DEN).max(QR_MIN_PX);
    let margin = (MARGIN_NUM * frame_h / MARGIN_DEN).max(MARGIN_MIN_PX);
    let x = frame_w.saturating_sub(margin).saturating_sub(side);
    let y = frame_h.saturating_sub(margin).saturating_sub(side);
    BurnGeom { side, margin, x, y }
}

/// Standard CRC-32 (IEEE 802.3 / ISO-HDLC — poly `0xEDB88320`, reflected, init
/// and xorout all-ones). This is the exact algorithm camera-box's `payload.rs`
/// uses via the `crc` crate's `CRC_32_ISO_HDLC`; its check value for the string
/// `"123456789"` is `0xCBF43926`. Kept dependency-free (the design offers "the
/// 256-entry table inline or the `crc32fast` crate" — inline keeps `sp-core`
/// dependency-free and unconditionally WASM-safe). The lookup table is built
/// once at const-eval time, so there is no hand-typed table to mistype.
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

/// Compute the CRC-32 of `data` (see [`CRC32_TABLE`]).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let idx = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    !crc
}

/// Encode the burn-id wire string `P{run_id}.{frame_id}.{gen_ts_ns}.{crc32}`,
/// where the CRC covers the dotted body `run_id.frame_id.gen_ts_ns` — byte-for-
/// byte identical to camera-box `Payload::encode`. `gen_ts_ns` is the frame's
/// generation wall time in NANOSECONDS.
pub fn payload(run_id: u32, frame_id: u32, gen_ts_ns: i64) -> String {
    let body = format!("{run_id}.{frame_id}.{gen_ts_ns}");
    let crc = crc32(body.as_bytes());
    format!("P{body}.{crc}")
}

/// Decode a burn-id wire string into `(run_id, frame_id, gen_ts_ns)`, returning
/// `None` on a malformed string or a CRC mismatch — mirrors camera-box
/// `Payload::decode`. Used by the fixture test and available for parity.
pub fn decode(s: &str) -> Option<(u32, u32, i64)> {
    let rest = s.strip_prefix('P')?;
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let run_id: u32 = parts[0].parse().ok()?;
    let frame_id: u32 = parts[1].parse().ok()?;
    let gen_ts_ns: i64 = parts[2].parse().ok()?;
    let crc: u32 = parts[3].parse().ok()?;
    let body = format!("{run_id}.{frame_id}.{gen_ts_ns}");
    if crc32(body.as_bytes()) != crc {
        return None;
    }
    Some((run_id, frame_id, gen_ts_ns))
}
