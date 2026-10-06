//! BT.709 limited range → full-range RGB, the one colour conversion of the
//! compositor (pure).
//!
//! The decoder's NV12 is 8-bit BT.709 limited range: Y 16–235, Cb/Cr 16–240
//! centred on 128. The render target is full-range BGRA. With
//! `Yn = (Y − 16)/219`, `Pb = (Cb − 128)/224`, `Pr = (Cr − 128)/224` and the
//! BT.709 weights Kr = 0.2126, Kb = 0.0722, Kg = 1 − Kr − Kb:
//!
//! - R = Yn + 2(1 − Kr)·Pr
//! - G = Yn − 2Kb(1 − Kb)/Kg·Pb − 2Kr(1 − Kr)/Kg·Pr
//! - B = Yn + 2(1 − Kb)·Pb
//!
//! The GPU samples the planes as UNORM, `y = Y/255` (and `u`, `v` alike), so
//! each channel is one affine row over `(y, u, v, 1)`:
//! [`BT709_LIMITED_TO_FULL`]. The rows go to the shader in its constant
//! buffer as f32 ([`matrix_f32`], `QuadConstants`), and the CPU reference
//! uses the same f32 values, so both apply the same numbers. The shader
//! saturates each channel to [0, 1].

/// BT.709's red luma weight.
pub const KR: f64 = 0.2126;

/// BT.709's blue luma weight.
pub const KB: f64 = 0.0722;

/// The rows of the affine transform: channel (R, G, B) =
/// `dot(row, (y, u, v, 1))`, with y, u, v the sampled UNORM values
/// (code / 255). Derived from [`KR`] / [`KB`] as in the module doc; the
/// tests recompute them and pin known colours.
pub const BT709_LIMITED_TO_FULL: [[f64; 4]; 3] = [
    [
        1.1643835616438356,
        0.0,
        1.7927410714285714,
        -0.9729450750163078,
    ],
    [
        1.1643835616438356,
        -0.21324861427372965,
        -0.5329093285594441,
        0.30148266547586217,
    ],
    [
        1.1643835616438356,
        2.112401785714286,
        0.0,
        -1.1334022178734506,
    ],
];

/// [`BT709_LIMITED_TO_FULL`] as the shader gets it: each value rounded to
/// f32.
pub fn matrix_f32() -> [[f32; 4]; 3] {
    BT709_LIMITED_TO_FULL.map(|row| row.map(|value| value as f32))
}

#[cfg(test)]
#[path = "color_tests.rs"]
mod tests;
