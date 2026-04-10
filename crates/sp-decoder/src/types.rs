//! Public frame types used by other crates.
//!
//! These types are **not** behind `cfg(windows)` so that crates consuming
//! decoded frames can compile on any platform.

/// Pixel format produced by the decoder.
///
/// Today only NV12 is produced because Windows Media Foundation's hardware
/// path negotiates NV12 natively and NDI accepts NV12 FourCC directly — no
/// intermediate BGRA conversion is performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// NV12 semi-planar: Y plane (height rows of `stride` bytes),
    /// immediately followed by interleaved UV plane (height/2 rows).
    Nv12,
}

/// Metadata describing the video stream of an opened media file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct VideoStreamInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format.
    pub pixel_format: PixelFormat,
    /// Frame rate numerator.
    pub frame_rate_num: u32,
    /// Frame rate denominator.
    pub frame_rate_den: u32,
}

/// A single decoded video frame.
#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    /// Raw pixel data in the layout required by `pixel_format`.
    pub data: Vec<u8>,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Row stride in bytes for the primary plane (Y plane for NV12).
    pub stride: u32,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Pixel format of `data`.
    pub pixel_format: PixelFormat,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_format_nv12_is_unique() {
        assert_eq!(PixelFormat::Nv12, PixelFormat::Nv12);
    }

    #[test]
    fn video_stream_info_round_trip() {
        let info = VideoStreamInfo {
            width: 1920,
            height: 1080,
            pixel_format: PixelFormat::Nv12,
            frame_rate_num: 30000,
            frame_rate_den: 1001,
        };
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.frame_rate_num, 30000);
        assert_eq!(info.frame_rate_den, 1001);
        assert_eq!(info.pixel_format, PixelFormat::Nv12);
    }

    #[test]
    fn decoded_video_frame_carries_pixel_format() {
        let f = DecodedVideoFrame {
            data: vec![0u8; 6],
            width: 2,
            height: 2,
            stride: 2,
            timestamp_ms: 0,
            pixel_format: PixelFormat::Nv12,
        };
        assert_eq!(f.pixel_format, PixelFormat::Nv12);
        // NV12 size: w * h * 3 / 2 = 2 * 2 * 3 / 2 = 6
        assert_eq!(f.data.len(), 6);
    }
}
