//! Tests for the cross-platform types and error module.

use sp_decoder::{DecodedAudioFrame, DecodedVideoFrame, DecoderError, PixelFormat};

// ---------------------------------------------------------------------------
// DecoderError Display tests
// ---------------------------------------------------------------------------

#[test]
fn error_display_com_init() {
    let e = DecoderError::ComInit("reason".into());
    assert_eq!(e.to_string(), "COM initialization failed: reason");
}

#[test]
fn error_display_source_reader() {
    let e = DecoderError::SourceReader("bad path".into());
    assert_eq!(e.to_string(), "Failed to create source reader: bad path");
}

#[test]
fn error_display_no_stream() {
    let e = DecoderError::NoStream("video");
    assert_eq!(e.to_string(), "No video stream available");
}

#[test]
fn error_display_read_sample() {
    let e = DecoderError::ReadSample("hr=0x80004005".into());
    assert_eq!(e.to_string(), "Sample read failed: hr=0x80004005");
}

#[test]
fn error_display_end_of_stream() {
    let e = DecoderError::EndOfStream;
    assert_eq!(e.to_string(), "End of stream");
}

#[test]
fn error_display_seek() {
    let e = DecoderError::Seek("invalid position".into());
    assert_eq!(e.to_string(), "Seek failed: invalid position");
}

#[test]
fn error_display_buffer_lock() {
    let e = DecoderError::BufferLock("null pointer".into());
    assert_eq!(e.to_string(), "Buffer lock failed: null pointer");
}

#[test]
fn error_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    // DecoderError should be Send + Sync since it only contains String / &'static str.
    assert_send_sync::<DecoderError>();
}

// ---------------------------------------------------------------------------
// DecodedVideoFrame tests
// ---------------------------------------------------------------------------

#[test]
fn video_frame_fields() {
    let frame = DecodedVideoFrame {
        data: vec![0u8; 1920 * 1080 * 4],
        width: 1920,
        height: 1080,
        stride: 1920 * 4,
        timestamp_ms: 42,
        pixel_format: PixelFormat::Nv12,
    };
    assert_eq!(frame.width, 1920);
    assert_eq!(frame.height, 1080);
    assert_eq!(frame.stride, 7680);
    assert_eq!(frame.timestamp_ms, 42);
    assert_eq!(frame.data.len(), 1920 * 1080 * 4);
}

#[test]
fn video_frame_clone() {
    let frame = DecodedVideoFrame {
        data: vec![0xAB; 16],
        width: 2,
        height: 2,
        stride: 8,
        timestamp_ms: 100,
        pixel_format: PixelFormat::Nv12,
    };
    let cloned = frame.clone();
    assert_eq!(cloned.data, frame.data);
    assert_eq!(cloned.timestamp_ms, frame.timestamp_ms);
}

#[test]
fn video_frame_debug() {
    let frame = DecodedVideoFrame {
        data: vec![],
        width: 0,
        height: 0,
        stride: 0,
        timestamp_ms: 0,
        pixel_format: PixelFormat::Nv12,
    };
    let dbg = format!("{frame:?}");
    assert!(dbg.contains("DecodedVideoFrame"));
}

// ---------------------------------------------------------------------------
// DecodedAudioFrame tests
// ---------------------------------------------------------------------------

#[test]
fn audio_frame_fields() {
    let frame = DecodedAudioFrame {
        data: vec![0.0f32; 1024],
        channels: 2,
        sample_rate: 48_000,
        timestamp_ms: 500,
    };
    assert_eq!(frame.channels, 2);
    assert_eq!(frame.sample_rate, 48_000);
    assert_eq!(frame.timestamp_ms, 500);
    assert_eq!(frame.data.len(), 1024);
}

#[test]
fn audio_frame_clone() {
    let frame = DecodedAudioFrame {
        data: vec![0.5, -0.5, 0.25, -0.25],
        channels: 2,
        sample_rate: 44_100,
        timestamp_ms: 10,
    };
    let cloned = frame.clone();
    assert_eq!(cloned.data, frame.data);
    assert_eq!(cloned.channels, frame.channels);
}

#[test]
fn audio_frame_debug() {
    let frame = DecodedAudioFrame {
        data: vec![],
        channels: 1,
        sample_rate: 16_000,
        timestamp_ms: 0,
    };
    let dbg = format!("{frame:?}");
    assert!(dbg.contains("DecodedAudioFrame"));
}
