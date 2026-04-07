//! NDI SDK FFI sender bindings for SongPlayer.
//!
//! Provides a safe, mockable interface for sending video and audio frames
//! over NDI. The NDI shared library is loaded at runtime via `libloading`,
//! so the crate compiles on any platform without the NDI SDK installed.

pub mod error;
pub mod ndi_sdk;
pub mod sender;
pub mod types;

// Re-export key public types for convenience.
pub use error::NdiError;
pub use ndi_sdk::NdiLib;
pub use sender::{AudioFrame, NdiBackend, NdiSender, RealNdiBackend, Tally, VideoFrame};
pub use types::{
    FRAME_FORMAT_PROGRESSIVE, FourCCAudioType, FourCCVideoType, NDI_SEND_TIMECODE_SYNTHESIZE,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    // FFI struct size / alignment sanity checks.
    // These ensure the #[repr(C)] layout hasn't accidentally changed.

    #[test]
    fn send_create_t_is_non_zero_size() {
        assert!(mem::size_of::<types::NDIlib_send_create_t>() > 0);
    }

    #[test]
    fn video_frame_v2_t_is_non_zero_size() {
        assert!(mem::size_of::<types::NDIlib_video_frame_v2_t>() > 0);
    }

    #[test]
    fn audio_frame_v3_t_is_non_zero_size() {
        assert!(mem::size_of::<types::NDIlib_audio_frame_v3_t>() > 0);
    }

    #[test]
    fn tally_t_is_small() {
        // Two bools — should be at most a few bytes.
        assert!(mem::size_of::<types::NDIlib_tally_t>() <= 4);
    }

    #[test]
    fn fourcc_video_bgra_value() {
        assert_eq!(FourCCVideoType::BGRA as u32, 0x4147_5242);
    }

    #[test]
    fn fourcc_audio_flt_interleaved_value() {
        assert_eq!(FourCCAudioType::FltInterleaved as u32, 0x0000_0001);
    }

    #[test]
    fn timecode_synthesize_is_i64_max() {
        assert_eq!(NDI_SEND_TIMECODE_SYNTHESIZE, i64::MAX);
    }

    #[test]
    fn frame_format_progressive_is_one() {
        assert_eq!(FRAME_FORMAT_PROGRESSIVE, 1);
    }

    #[test]
    fn ndi_lib_load_fails_gracefully_without_sdk() {
        // On CI / dev machines without NDI SDK, this must return an error, not panic.
        let result = NdiLib::load();
        match result {
            Err(NdiError::LibraryNotFound(_)) => {} // expected
            Err(other) => panic!("Expected LibraryNotFound, got: {other}"),
            Ok(_) => panic!("Expected error when NDI SDK is not installed"),
        }
    }

    #[test]
    fn ndi_error_display() {
        let e = NdiError::LibraryNotFound("test.so".into());
        assert!(format!("{e}").contains("test.so"));

        let e = NdiError::SymbolNotFound("NDIlib_foo".into());
        assert!(format!("{e}").contains("NDIlib_foo"));

        let e = NdiError::InitFailed;
        assert!(format!("{e}").contains("initialize"));
    }
}
