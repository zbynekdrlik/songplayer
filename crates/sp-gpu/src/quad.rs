//! The shader constants of one quad (pure): its rectangle in normalized
//! device coordinates, the part of the picture it shows, the YUV → RGB rows
//! and its blend weight.
//!
//! The HLSL side (`compose.hlsl`, `cbuffer Quad : register(b0)`) is six
//! float4 registers in this order — change both together:
//!
//! - `rect`: left, top, right, bottom in NDC;
//! - `uv_rect` (#267): left, top, right, bottom in texture coordinates;
//! - `to_r`, `to_g`, `to_b`: the rows of [`matrix_f32`];
//! - `weight`: the blend weight in x, then zeros.

use sp_core::fit::{Fill, Placement};

use crate::color::matrix_f32;

/// The constant buffer's size: six float4 registers.
pub const QUAD_CONSTANTS_BYTES: usize = 96;

/// The constants of one quad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadConstants {
    /// The quad in normalized device coordinates: left, top, right, bottom.
    pub rect: [f32; 4],
    /// #267: the part of the picture the quad shows, in texture coordinates:
    /// left, top, right, bottom ([`uv_rect`]); the vertex shader gives each
    /// corner its edge.
    pub uv_rect: [f32; 4],
    /// The YUV → RGB rows ([`matrix_f32`]).
    pub matrix: [[f32; 4]; 3],
    /// The blend weight.
    pub weight: f32,
}

impl QuadConstants {
    /// The constants of a quad that draws the `uv_rect` part of a picture
    /// into `place` of a `target_width`×`target_height` render target at
    /// `weight` (#239: the compositor's own size, 3840×2160 for MAX,
    /// 1920×1080 for `SP-program`; #267: a layer's own `uv_rect`).
    pub fn new(
        place: Placement,
        uv_rect: [f32; 4],
        weight: f32,
        target_width: u32,
        target_height: u32,
    ) -> Self {
        Self {
            rect: ndc_rect(place, target_width, target_height),
            uv_rect,
            matrix: matrix_f32(),
            weight,
        }
    }

    /// The constant buffer's bytes: 24 little-endian f32, as the six
    /// registers lay them out.
    pub fn to_bytes(&self) -> [u8; QUAD_CONSTANTS_BYTES] {
        // 24 = QUAD_CONSTANTS_BYTES / 4, written as a literal: a computed
        // length is a mutant the zips below cannot see.
        let mut floats = [0f32; 24];
        let registers = [
            self.rect,
            self.uv_rect,
            self.matrix[0],
            self.matrix[1],
            self.matrix[2],
        ];
        for (register, values) in floats.chunks_exact_mut(4).zip(registers) {
            register.copy_from_slice(&values);
        }
        floats[20] = self.weight;
        let mut bytes = [0u8; QUAD_CONSTANTS_BYTES];
        for (out, value) in bytes.chunks_exact_mut(4).zip(floats) {
            out.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

/// A `width`×`height` target's pixel rectangle `place` in normalized device
/// coordinates `[left, top, right, bottom]`: x = 2·px/width − 1,
/// y = 1 − 2·py/height (NDC's y points up). The edges land on pixel edges,
/// so the quad covers exactly the pixels whose centres are inside `place`.
pub fn ndc_rect(place: Placement, width: u32, height: u32) -> [f32; 4] {
    let x = |px: u32| (2.0 * f64::from(px) / f64::from(width) - 1.0) as f32;
    let y = |py: u32| (1.0 - 2.0 * f64::from(py) / f64::from(height)) as f32;
    [
        x(place.off_x),
        y(place.off_y),
        x(place.off_x + place.w),
        y(place.off_y + place.h),
    ]
}

/// #267: the part of the picture a quad placed by `fill` shows, in texture
/// coordinates `[left, top, right, bottom]`: every column, and rows
/// `crop_top..crop_top + h` of the `scaled_h` rows the picture was scaled
/// to, as fractions of them. A whole picture ([`Fill::whole`]) is exactly
/// `[0, 0, 1, 1]`; a degenerate one (no rows) reads `[0, 0, 1, 0]` (its
/// quad covers nothing).
pub fn uv_rect(fill: Fill) -> [f32; 4] {
    let rows = f64::from(fill.scaled_h.max(1));
    let v = |row: u32| (f64::from(row) / rows) as f32;
    [0.0, v(fill.crop_top), 1.0, v(fill.crop_top + fill.place.h)]
}

/// #279 P2: `uv` (the part of a `picture`-sized picture a quad shows,
/// [`uv_rect`]) in the coordinates of the `texture` the picture lies in, at
/// its top-left: a ring slot's texture has even sides, so it can be one
/// column wider and one row taller than its picture. u scales by width /
/// texture width, v by height / texture height: exactly `uv` when the
/// texture is the picture's size (an uploaded picture, an even one). The
/// quad then spans the picture's own texels; the bilinear filter clamps at
/// the TEXTURE's edge, so an enlarged odd picture's last column (row)
/// blends up to half a texel of the slot's extra one.
pub fn uv_in_texture(uv: [f32; 4], picture: (u32, u32), texture: (u32, u32)) -> [f32; 4] {
    let scale = |side: u32, texture: u32| side as f32 / texture.max(1) as f32;
    let (su, sv) = (scale(picture.0, texture.0), scale(picture.1, texture.1));
    [uv[0] * su, uv[1] * sv, uv[2] * su, uv[3] * sv]
}

#[cfg(test)]
#[path = "quad_tests.rs"]
mod tests;
