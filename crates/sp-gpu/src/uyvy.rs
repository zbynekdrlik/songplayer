//! #279 P1a: the UYVY pass of the compositor (pure): its colour conversion,
//! the CPU model of what it writes, and its staging ring. The Direct3D calls
//! are `win/uyvy.rs`; the shader is `uyvy.hlsl`.
//!
//! NDI's `SP-program` takes CPU buffers only, and UYVY is the format NDI
//! sends without converting it again (epic #279 plan §4.4). The compositor
//! already draws the FHD program as a full-range BGRA target, so the pass
//! converts THAT target on the GPU: a full-target triangle over a
//! `R8G8B8A8_UNORM` texture of `width / 2 × height` texels, texel (x, y)
//! written from the target's pixels (2x, y) and (2x + 1, y) as
//! `(U, Y0, V, Y1)`. In memory a texel's four bytes are then U Y0 V Y1:
//! one UYVY pixel pair. One `CopyResource` puts it in a CPU-readable staging
//! texture, in the same compose, before its one wait, so the readback map
//! never waits (`D3D11_MAP_FLAG_DO_NOT_WAIT`).
//!
//! **The conversion** ([`RGB_FULL_TO_BT709_LIMITED`]) is BT.709 limited
//! range from full-range RGB, the inverse of
//! [`BT709_LIMITED_TO_FULL`](crate::BT709_LIMITED_TO_FULL): with
//! `Y = Kr·r + Kg·g + Kb·b` (r, g, b = code / 255),
//!
//! - Y′ = (16 + 219·Y) / 255
//! - Cb = (128 + 224·(b − Y) / (2(1 − Kb))) / 255
//! - Cr = (128 + 224·(r − Y) / (2(1 − Kr))) / 255
//!
//! Each is one affine row over `(r, g, b, 1)`, already divided by 255: the
//! UNORM value the render target stores. The rows reach the shader as f32
//! in its constant buffer ([`rows_f32`], [`constants_bytes`]), and the CPU
//! model uses the same f32 values.
//!
//! **Chroma** is the conversion of the two pixels' AVERAGE RGB, with no
//! filter tap: the conversion is linear, so that equals the average of
//! their Cb (and Cr). That is a chroma sample centred between the pair, not
//! BT.709's co-sited one on its left pixel: the design's choice, two exact
//! `Load`s and no filter tap.
//!
//! **How far a correct GPU may be from [`reference`]: [`UYVY_TOLERANCE`],
//! one code.** The pass reads its two pixels with `Texture2D.Load` (no
//! filtering: each UNORM byte becomes exactly `code / 255`), computes in
//! f32 like [`pack_pair`], and rounds each output once. The GPU may order
//! its sums differently or fuse them, and its float → UNORM rounding may be
//! 0.6 ULP off: near a half code that moves the stored byte by one, never
//! more. The WARP test compares the pass with the model computed from the
//! SAME target's BGRA readback, so the composite's own tolerance never adds
//! to it.

use crate::error::GpuError;
use crate::probe::DXGI_ERROR_WAS_STILL_DRAWING;
use crate::reference::unorm8;

/// The rows of the conversion: (Y′, Cb, Cr) = `dot(row, (r, g, b, 1))`,
/// with r, g, b the full-range UNORM values (code / 255) and each result
/// the limited-range code / 255. Derived from [`KR`](crate::KR) /
/// [`KB`](crate::KB) as in the module doc; the tests recompute them and
/// check they invert [`BT709_LIMITED_TO_FULL`](crate::BT709_LIMITED_TO_FULL).
pub const RGB_FULL_TO_BT709_LIMITED: [[f64; 4]; 3] = [
    [
        0.18258588235294118,
        0.6142305882352941,
        0.06200705882352941,
        0.06274509803921569,
    ],
    [
        -0.10064373237978098,
        -0.3385719538947288,
        0.4392156862745098,
        0.5019607843137255,
    ],
    [
        0.4392156862745098,
        -0.3989421625902075,
        -0.040273523684302266,
        0.5019607843137255,
    ],
];

/// [`RGB_FULL_TO_BT709_LIMITED`] as the shader gets it: each value rounded
/// to f32, rows Y′, Cb, Cr.
pub fn rows_f32() -> [[f32; 4]; 3] {
    RGB_FULL_TO_BT709_LIMITED.map(|row| row.map(|value| value as f32))
}

/// The pass's constant buffer: three float4 registers (`uyvy.hlsl`'s
/// `cbuffer Uyvy`: `to_y`, `to_u`, `to_v`).
pub const UYVY_CONSTANTS_BYTES: usize = 48;

/// The constant buffer's bytes: the rows of [`rows_f32`] in order, each
/// value a little-endian f32.
pub fn constants_bytes() -> [u8; UYVY_CONSTANTS_BYTES] {
    let mut bytes = [0u8; UYVY_CONSTANTS_BYTES];
    let values = rows_f32().into_iter().flatten();
    for (out, value) in bytes.chunks_exact_mut(4).zip(values) {
        out.copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// How many codes a correct GPU's UYVY byte may differ from
/// [`reference`]'s (module doc).
pub const UYVY_TOLERANCE: u8 = 1;

/// One UNORM channel byte as the shader reads it: `code / 255` in f32.
fn unorm(code: u8) -> f32 {
    f32::from(code) / 255.0
}

/// A BGRA pixel's (r, g, b) as the shader reads them.
fn rgb(bgra: [u8; 4]) -> [f32; 3] {
    [unorm(bgra[2]), unorm(bgra[1]), unorm(bgra[0])]
}

/// One row of the conversion applied to `rgb` in f32, summed left to right
/// as written in the shader's `dot`.
fn convert(row: [f32; 4], rgb: [f32; 3]) -> u8 {
    let value = rgb[0] * row[0] + rgb[1] * row[1] + rgb[2] * row[2] + row[3];
    unorm8(f64::from(value))
}

/// The pass's output for one pixel pair, the CPU model of `uyvy.hlsl`:
/// `[U, Y0, V, Y1]` (the bytes of one UYVY pair) from the BGRA pixels
/// `left` (2x) and `right` (2x + 1). Each pixel's own Y′; U and V of the
/// pair's average RGB; f32 maths, then the render target's rounding
/// (`reference::unorm8`).
pub fn pack_pair(left: [u8; 4], right: [u8; 4]) -> [u8; 4] {
    let [to_y, to_u, to_v] = rows_f32();
    let (left, right) = (rgb(left), rgb(right));
    let mean = [0, 1, 2].map(|c| (left[c] + right[c]) * 0.5);
    [
        convert(to_u, mean),
        convert(to_y, left),
        convert(to_v, mean),
        convert(to_y, right),
    ]
}

/// What the pass writes for a `width`×`height` BGRA frame (`bgra`, rows
/// packed, as `Compositor::read_back_target` returns them): `height` rows
/// of `width / 2` pairs ([`pack_pair`]), 2 bytes a pixel, rows packed. The
/// width must be even (the compositor refuses another, [`uyvy_size`]): an
/// odd last column would be left out. A frame shorter than `height` rows
/// gives the rows it has.
pub fn reference(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
    let row = width as usize * 4;
    let mut uyvy = Vec::new();
    // `max(1)`: a zero width has no pixels (and `chunks_exact(0)` panics).
    for line in bgra.chunks_exact(row.max(1)).take(height as usize) {
        for pair in line.chunks_exact(8) {
            let left = [pair[0], pair[1], pair[2], pair[3]];
            let right = [pair[4], pair[5], pair[6], pair[7]];
            uyvy.extend(pack_pair(left, right));
        }
    }
    uyvy
}

/// The size of the pass's `R8G8B8A8_UNORM` texture for a target of
/// `width`×`height`: one texel per pixel pair, `(width / 2, height)`. A
/// target of odd width has a pixel no pair holds:
/// [`GpuError::UyvyOddWidth`].
pub fn uyvy_size(width: u32, height: u32) -> Result<(u32, u32), GpuError> {
    if !width.is_multiple_of(2) {
        return Err(GpuError::UyvyOddWidth { width });
    }
    Ok((width / 2, height))
}

/// The call a failed readback map names.
pub const MAP_CALL: &str = "Map (the UYVY staging, DO_NOT_WAIT)";

/// The call a `DXGI_ERROR_WAS_STILL_DRAWING` readback map names: the copy
/// was not finished, which the compose's own wait rules out, so it is a
/// failure, never a wait.
pub const STILL_DRAWING_CALL: &str =
    "Map (the UYVY staging, DO_NOT_WAIT: DXGI_ERROR_WAS_STILL_DRAWING, the copy was not done)";

/// The error of a failed readback map of HRESULT `hresult`: a
/// [`GpuError::Api`] naming `DXGI_ERROR_WAS_STILL_DRAWING`
/// ([`STILL_DRAWING_CALL`]) for that code, else
/// [`GpuError::from_hresult`] of [`MAP_CALL`] (a lost device is
/// [`GpuError::DeviceLost`]).
pub fn map_failed(hresult: u32) -> GpuError {
    match hresult {
        DXGI_ERROR_WAS_STILL_DRAWING => GpuError::Api {
            call: STILL_DRAWING_CALL,
            hresult,
        },
        _ => GpuError::from_hresult(MAP_CALL, hresult),
    }
}

/// How many staging textures the readback rotates through (plan §4.4,
/// §5.5): the compose copies into one while the newest finished copy stays
/// readable in another, and a third is spare if a map is ever deferred.
pub const UYVY_STAGING: usize = 3;

/// Which staging texture the next compose copies into and which one holds
/// the newest finished copy. A copy counts only once its compose finished
/// ([`StagingRing::written`]), so a failed compose leaves the newest
/// picture readable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StagingRing {
    /// The slot the next compose copies into.
    next: usize,
    /// The slot the last finished compose copied into; `None` before one.
    newest: Option<usize>,
}

impl StagingRing {
    /// No copy yet; the first compose copies into slot 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// The slot this compose copies into (never the newest one).
    pub fn slot_to_write(&self) -> usize {
        self.next
    }

    /// The compose that copied into [`slot_to_write`](Self::slot_to_write)
    /// finished: that slot now holds the newest picture, and the next
    /// compose copies into the slot after it (wrapping at
    /// [`UYVY_STAGING`]).
    pub fn written(&mut self) {
        self.newest = Some(self.next);
        self.next = (self.next + 1) % UYVY_STAGING;
    }

    /// The slot holding the newest finished copy, `None` before the first:
    /// what a readback maps. Reading changes nothing.
    pub fn newest(&self) -> Option<usize> {
        self.newest
    }
}

#[cfg(test)]
#[path = "uyvy_tests.rs"]
mod tests;
