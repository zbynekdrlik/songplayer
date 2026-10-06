//! The shader constants of one quad (pure): its rectangle in normalized
//! device coordinates, the YUV → RGB rows and its blend weight.
//!
//! The HLSL side (`compose.hlsl`, `cbuffer Quad : register(b0)`) is five
//! float4 registers in this order — change both together:
//!
//! - `rect`: left, top, right, bottom in NDC;
//! - `to_r`, `to_g`, `to_b`: the rows of [`matrix_f32`];
//! - `weight`: the blend weight in x, then zeros.

use sp_core::fit::Placement;

use crate::color::matrix_f32;
use crate::composition::{CANVAS_HEIGHT, CANVAS_WIDTH};

/// The constant buffer's size: five float4 registers.
pub const QUAD_CONSTANTS_BYTES: usize = 80;

/// The constants of one quad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadConstants {
    /// The quad in normalized device coordinates: left, top, right, bottom.
    pub rect: [f32; 4],
    /// The YUV → RGB rows ([`matrix_f32`]).
    pub matrix: [[f32; 4]; 3],
    /// The blend weight.
    pub weight: f32,
}

impl QuadConstants {
    /// The constants of a quad that draws a picture into `place` of the
    /// canvas at `weight`.
    pub fn new(place: Placement, weight: f32) -> Self {
        Self {
            rect: ndc_rect(place, CANVAS_WIDTH, CANVAS_HEIGHT),
            matrix: matrix_f32(),
            weight,
        }
    }

    /// The constant buffer's bytes: 20 little-endian f32, as the five
    /// registers lay them out.
    pub fn to_bytes(&self) -> [u8; QUAD_CONSTANTS_BYTES] {
        // 20 = QUAD_CONSTANTS_BYTES / 4, written as a literal: a computed
        // length is a mutant the zips below cannot see.
        let mut floats = [0f32; 20];
        let registers = [self.rect, self.matrix[0], self.matrix[1], self.matrix[2]];
        for (register, values) in floats.chunks_exact_mut(4).zip(registers) {
            register.copy_from_slice(&values);
        }
        floats[16] = self.weight;
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

#[cfg(test)]
#[path = "quad_tests.rs"]
mod tests;
