//! Public frame types used by other crates.
//!
//! These types are **not** behind `cfg(windows)` so that crates consuming
//! decoded frames can compile on any platform.

/// A single decoded video frame in BGRA pixel format.
#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    /// Raw BGRA pixel data.
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
}

/// A chunk of decoded audio as interleaved f32 PCM samples.
#[derive(Debug, Clone)]
pub struct DecodedAudioFrame {
    /// Interleaved f32 PCM sample data.
    pub data: Vec<f32>,
    /// Number of audio channels.
    pub channels: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
}
