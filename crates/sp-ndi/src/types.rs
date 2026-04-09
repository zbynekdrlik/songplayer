//! FFI types for the NDI SDK sender API.
//!
//! These are `#[repr(C)]` structs matching the NDI SDK C headers.
//! They compile on any platform — no link-time NDI dependency.

use std::ffi::c_char;

// ---------------------------------------------------------------------------
// Opaque handle
// ---------------------------------------------------------------------------

/// Opaque handle returned by `NDIlib_send_create`.
#[allow(non_camel_case_types)]
pub enum NDIlib_send_instance_t {}

// ---------------------------------------------------------------------------
// FourCC types
// ---------------------------------------------------------------------------

/// Video FourCC pixel format identifiers.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FourCCVideoType {
    /// BGRA 8-bit per channel (NDI default for sending).
    /// FourCC('B','G','R','A') = 0x41524742
    BGRA = 0x4152_4742,
}

/// Audio FourCC format identifiers.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FourCCAudioType {
    /// 32-bit float interleaved audio.
    FltInterleaved = 0x0000_0001,
}

// ---------------------------------------------------------------------------
// Frame format
// ---------------------------------------------------------------------------

/// Frame format type constants.
pub const FRAME_FORMAT_PROGRESSIVE: i32 = 1;

/// Synthesized timecode — NDI generates timestamps automatically.
pub const NDI_SEND_TIMECODE_SYNTHESIZE: i64 = i64::MAX;

// ---------------------------------------------------------------------------
// Sender creation descriptor
// ---------------------------------------------------------------------------

/// Passed to `NDIlib_send_create` to configure a new sender instance.
#[repr(C)]
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct NDIlib_send_create_t {
    /// Name of this NDI source on the network (UTF-8, null-terminated).
    pub p_ndi_name: *const c_char,
    /// Comma-separated group list, or null for the default group.
    pub p_groups: *const c_char,
    /// If true, `send_send_video_v2` blocks to pace output at the declared frame rate.
    pub clock_video: bool,
    /// If true, audio sending blocks to pace output.
    pub clock_audio: bool,
}

// SAFETY: The struct only contains raw pointers + booleans.
// Callers must ensure the pointed-to strings live long enough.
unsafe impl Send for NDIlib_send_create_t {}
unsafe impl Sync for NDIlib_send_create_t {}

// ---------------------------------------------------------------------------
// Video frame
// ---------------------------------------------------------------------------

/// Video frame descriptor for `NDIlib_send_send_video_v2`.
#[repr(C)]
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct NDIlib_video_frame_v2_t {
    /// Horizontal resolution in pixels.
    pub xres: i32,
    /// Vertical resolution in pixels.
    pub yres: i32,
    /// Pixel format.
    pub four_cc: FourCCVideoType,
    /// Frame rate numerator (e.g. 30000 for 29.97).
    pub frame_rate_n: i32,
    /// Frame rate denominator (e.g. 1001 for 29.97).
    pub frame_rate_d: i32,
    /// Picture aspect ratio (0.0 = square pixels).
    pub picture_aspect_ratio: f32,
    /// Frame format (use [`FRAME_FORMAT_PROGRESSIVE`]).
    pub frame_format_type: i32,
    /// Timecode in 100-ns intervals, or [`NDI_SEND_TIMECODE_SYNTHESIZE`].
    pub timecode: i64,
    /// Pointer to pixel data.
    pub p_data: *const u8,
    /// Number of bytes per scan line.
    pub line_stride_in_bytes: i32,
    /// Optional XML metadata string (null-terminated), or null.
    pub p_metadata: *const c_char,
    /// Timestamp in 100-ns intervals (0 = ignored).
    pub timestamp: i64,
}

unsafe impl Send for NDIlib_video_frame_v2_t {}
unsafe impl Sync for NDIlib_video_frame_v2_t {}

// ---------------------------------------------------------------------------
// Audio frame
// ---------------------------------------------------------------------------

/// Audio frame descriptor for `NDIlib_send_send_audio_v3`.
#[repr(C)]
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub struct NDIlib_audio_frame_v3_t {
    /// Sample rate in Hz (e.g. 48000).
    pub sample_rate: i32,
    /// Number of audio channels.
    pub no_channels: i32,
    /// Number of samples per channel.
    pub no_samples: i32,
    /// Timecode in 100-ns intervals, or [`NDI_SEND_TIMECODE_SYNTHESIZE`].
    pub timecode: i64,
    /// Audio data format.
    pub four_cc: FourCCAudioType,
    /// Pointer to interleaved float sample data.
    pub p_data: *const f32,
    /// Stride in bytes between channels (for planar formats; 0 for interleaved).
    pub channel_stride_in_bytes: i32,
    /// Optional XML metadata string (null-terminated), or null.
    pub p_metadata: *const c_char,
    /// Timestamp in 100-ns intervals (0 = ignored).
    pub timestamp: i64,
}

unsafe impl Send for NDIlib_audio_frame_v3_t {}
unsafe impl Sync for NDIlib_audio_frame_v3_t {}

// ---------------------------------------------------------------------------
// Tally
// ---------------------------------------------------------------------------

/// Tally state reported by `NDIlib_send_get_tally`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
#[allow(non_camel_case_types)]
pub struct NDIlib_tally_t {
    /// Source is on program (live) output.
    pub on_program: bool,
    /// Source is on preview output.
    pub on_preview: bool,
}
