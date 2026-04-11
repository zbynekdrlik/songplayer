//! Interleaved → planar float audio conversion for NDI FLTP output.
//!
//! NDI's `NDIlib_FourCC_audio_type_FLTP` expects the audio buffer laid out
//! channel-by-channel, e.g. stereo:
//!
//!   `[L0 L1 L2 … L_{n-1}][R0 R1 R2 … R_{n-1}]`
//!
//! Windows Media Foundation delivers interleaved:
//!
//!   `[L0 R0 L1 R1 L2 R2 … L_{n-1} R_{n-1}]`
//!
//! This module provides a zero-allocation-in-steady-state conversion that
//! reuses a caller-owned scratch `Vec<f32>`.

/// Convert interleaved multi-channel audio into planar layout.
///
/// * `interleaved` — `[ch0_s0, ch1_s0, …, ch_{c-1}_s0, ch0_s1, …]`
/// * `channels` — number of channels (must be > 0 and must divide `interleaved.len()`)
/// * `out` — destination scratch buffer; cleared and resized to hold the output
///
/// If `channels == 0` or `interleaved` is empty, `out` is cleared and the
/// function returns.
pub fn deinterleave(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    if channels == 0 || interleaved.is_empty() {
        out.clear();
        return;
    }
    let samples_per_channel = interleaved.len() / channels;
    let total = channels * samples_per_channel;
    out.clear();
    out.resize(total, 0.0);
    for ch in 0..channels {
        for s in 0..samples_per_channel {
            out[ch * samples_per_channel + s] = interleaved[s * channels + ch];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_clears_output() {
        let mut out = vec![1.0, 2.0, 3.0];
        deinterleave(&[], 2, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn zero_channels_clears_output() {
        let mut out = vec![1.0];
        deinterleave(&[1.0, 2.0], 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn mono_is_passthrough() {
        let input = vec![1.0, 2.0, 3.0, 4.0];
        let mut out = Vec::new();
        deinterleave(&input, 1, &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn stereo_four_samples() {
        // Interleaved: L0 R0 L1 R1 L2 R2 L3 R3
        let input = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut out = Vec::new();
        deinterleave(&input, 2, &mut out);
        // Planar: L0 L1 L2 L3 R0 R1 R2 R3
        assert_eq!(out, vec![1.0, 3.0, 5.0, 7.0, 2.0, 4.0, 6.0, 8.0]);
    }

    #[test]
    fn six_channel_preserves_sample_count_and_order() {
        // 2 samples per channel, 6 channels = 5.1 layout
        // Interleaved: s0(c0..c5) s1(c0..c5)
        let input: Vec<f32> = (0..12).map(|x| x as f32).collect();
        let mut out = Vec::new();
        deinterleave(&input, 6, &mut out);
        // Planar: c0_s0 c0_s1 c1_s0 c1_s1 … c5_s0 c5_s1
        // c0: input[0], input[6]
        // c1: input[1], input[7]
        // …
        assert_eq!(
            out,
            vec![
                0.0, 6.0, // c0
                1.0, 7.0, // c1
                2.0, 8.0, // c2
                3.0, 9.0, // c3
                4.0, 10.0, // c4
                5.0, 11.0, // c5
            ]
        );
    }

    #[test]
    fn preserves_exact_float_bits() {
        // Use non-round bit patterns to catch any accidental arithmetic.
        let a = f32::from_bits(0x3E8A_3D71); // ~0.27
        let b = f32::from_bits(0xBF19_999A); // ~-0.6
        let c = f32::from_bits(0x4049_0FDB); // ~3.1416
        let d = f32::from_bits(0xC0A0_0000); // -5.0
        let input = vec![a, b, c, d];
        let mut out = Vec::new();
        deinterleave(&input, 2, &mut out);
        // Stereo: L0=a L1=c R0=b R1=d
        assert_eq!(out[0].to_bits(), a.to_bits());
        assert_eq!(out[1].to_bits(), c.to_bits());
        assert_eq!(out[2].to_bits(), b.to_bits());
        assert_eq!(out[3].to_bits(), d.to_bits());
    }

    #[test]
    fn reuses_scratch_buffer_capacity_on_second_call() {
        let mut out = Vec::with_capacity(16);
        let cap_before = out.capacity();
        // First call: 4 samples × 2ch = 8 floats
        deinterleave(&[1.0; 8], 2, &mut out);
        assert_eq!(out.len(), 8);
        // Second call: 2 samples × 2ch = 4 floats (smaller, must not realloc)
        deinterleave(&[2.0; 4], 2, &mut out);
        assert_eq!(out.len(), 4);
        assert!(out.capacity() >= cap_before);
        // Third call: grow to 16 — may or may not realloc, both are fine.
        deinterleave(&[3.0; 16], 2, &mut out);
        assert_eq!(out.len(), 16);
    }
}
