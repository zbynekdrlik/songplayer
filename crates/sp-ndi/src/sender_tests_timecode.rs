//! Timecode plumbing tests for the NDI sender (#146).
//!
//! Verifies that `timecode_100ns` flows from `VideoFrame`/`AudioFrame`
//! through the backend, and that `None` resolves to the SYNTHESIZE marker
//! (`i64::MAX`) — i.e. what `RealNdiBackend` would write into
//! `NDIlib_*_frame.timecode`. Wired via
//! `#[cfg(test)] #[path = "sender_tests_timecode.rs"] mod sender_tests_timecode;`.

use super::*;
use crate::types::{FourCCVideoType, NDI_SEND_TIMECODE_SYNTHESIZE};
use std::sync::Arc;
use test_util::MockNdiBackend;

fn nv12_frame(tc: Option<i64>) -> VideoFrame {
    VideoFrame {
        data: vec![0u8; 4 * 2 * 3 / 2],
        width: 4,
        height: 2,
        stride: 4,
        frame_rate_n: 30,
        frame_rate_d: 1,
        pixel_format: PixelFormat::Nv12,
        timecode_100ns: tc,
    }
}

#[test]
fn video_some_timecode_is_recorded_verbatim() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "TC", false, false).unwrap();
    let frame = nv12_frame(Some(333_333));
    // SAFETY: `frame` outlives this call; a flush runs on drop.
    unsafe { sender.send_video_async(&frame) };
    assert_eq!(backend.video_timecodes(), vec![333_333]);
}

#[test]
fn video_none_records_synthesize_marker() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "TC", false, false).unwrap();
    let frame = nv12_frame(None);
    // SAFETY: `frame` outlives this call; a flush runs on drop.
    unsafe { sender.send_video_async(&frame) };
    assert_eq!(
        backend.video_timecodes(),
        vec![NDI_SEND_TIMECODE_SYNTHESIZE]
    );
}

#[test]
fn sync_video_records_timecode() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "TC", false, false).unwrap();
    sender.send_video(&nv12_frame(Some(166_666)));
    assert_eq!(backend.video_timecodes(), vec![166_666]);
}

#[test]
fn audio_some_timecode_is_recorded_verbatim() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "TC", false, false).unwrap();
    let frame = AudioFrame {
        data: vec![0.1, 0.2, 0.3, 0.4],
        channels: 2,
        sample_rate: 48000,
        timecode_100ns: Some(1_000_000_000),
    };
    sender.send_audio(&frame);
    assert_eq!(backend.audio_timecodes(), vec![1_000_000_000]);
}

#[test]
fn audio_none_records_synthesize_marker() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "TC", false, false).unwrap();
    let frame = AudioFrame {
        data: vec![0.1, 0.2],
        channels: 2,
        sample_rate: 48000,
        timecode_100ns: None,
    };
    sender.send_audio(&frame);
    assert_eq!(
        backend.audio_timecodes(),
        vec![NDI_SEND_TIMECODE_SYNTHESIZE]
    );
}

// --- Direct tests of the REAL write path (`RealNdiBackend::build_video_frame`).
// The mock re-implements `unwrap_or(SYNTHESIZE)`, so a bug in the real struct
// builder would slip past the mock-driven tests above. These call the real
// builder directly and assert the `NDIlib_video_frame_v2_t` fields. It is pure
// struct construction (no NDI SDK / DLL) so it runs on the Linux CI runner.

#[test]
fn build_video_frame_writes_some_timecode_and_zero_timestamp() {
    let frame = RealNdiBackend::build_video_frame(
        FourCCVideoType::NV12,
        1920,
        1080,
        1920,
        30,
        1,
        std::ptr::null(),
        Some(1_234_567_890),
    );
    assert_eq!(frame.timecode, 1_234_567_890);
    assert_eq!(frame.timestamp, 0);
}

#[test]
fn build_video_frame_none_writes_synthesize_marker() {
    let frame = RealNdiBackend::build_video_frame(
        FourCCVideoType::NV12,
        1920,
        1080,
        1920,
        30,
        1,
        std::ptr::null(),
        None,
    );
    assert_eq!(frame.timecode, NDI_SEND_TIMECODE_SYNTHESIZE);
    assert_eq!(frame.timestamp, 0);
}
