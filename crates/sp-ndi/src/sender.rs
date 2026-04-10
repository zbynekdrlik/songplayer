//! High-level NDI sender with a mockable backend trait.

use std::collections::HashMap;
use std::ffi::CString;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, info};

use crate::error::NdiError;
use crate::ndi_sdk::NdiLib;
use crate::types::{
    FRAME_FORMAT_PROGRESSIVE, FourCCAudioType, FourCCVideoType, NDI_SEND_TIMECODE_SYNTHESIZE,
    NDIlib_audio_frame_v3_t, NDIlib_send_create_t, NDIlib_send_instance_t, NDIlib_tally_t,
    NDIlib_video_frame_v2_t,
};

// ---------------------------------------------------------------------------
// Safe public frame types
// ---------------------------------------------------------------------------

/// A video frame ready to send over NDI.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// BGRA pixel data.
    pub data: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per scan line (usually `width * 4` for BGRA).
    pub stride: u32,
    /// Frame rate numerator.
    pub frame_rate_n: i32,
    /// Frame rate denominator.
    pub frame_rate_d: i32,
}

/// An audio frame ready to send over NDI.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Interleaved float samples.
    pub data: Vec<f32>,
    /// Number of audio channels.
    pub channels: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
}

/// Tally state — whether this source is on program / preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub on_program: bool,
    pub on_preview: bool,
}

// ---------------------------------------------------------------------------
// Backend trait (for mockability)
// ---------------------------------------------------------------------------

/// Abstraction over the NDI SDK for testing.
pub trait NdiBackend: Send + Sync {
    /// Create a new sender instance. Returns an opaque handle ID.
    fn send_create(&self, name: &str) -> Result<usize, NdiError>;
    /// Destroy a sender instance.
    fn send_destroy(&self, handle: usize);
    /// Send a video frame.
    #[allow(clippy::too_many_arguments)]
    fn send_video(
        &self,
        handle: usize,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
    );
    /// Send an audio frame.
    fn send_audio(
        &self,
        handle: usize,
        sample_rate: i32,
        channels: i32,
        samples: i32,
        data: &[f32],
    );
    /// Query tally state. Returns `None` if the timeout expired with no change.
    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)>;
}

// ---------------------------------------------------------------------------
// Real backend (wraps NdiLib)
// ---------------------------------------------------------------------------

/// Production [`NdiBackend`] backed by the real NDI SDK via [`NdiLib`].
pub struct RealNdiBackend {
    lib: Arc<NdiLib>,
    next_id: AtomicUsize,
    /// Map of handle ID → raw NDI sender pointer.
    handles: Mutex<HashMap<usize, *mut NDIlib_send_instance_t>>,
}

// SAFETY: The raw pointers in `handles` are only dereferenced through NDI SDK
// calls which are thread-safe per-instance. The Mutex serialises map access.
unsafe impl Send for RealNdiBackend {}
unsafe impl Sync for RealNdiBackend {}

impl RealNdiBackend {
    /// Create a new backend from an already-loaded NDI SDK.
    pub fn new(lib: Arc<NdiLib>) -> Self {
        Self {
            lib,
            next_id: AtomicUsize::new(1),
            handles: Mutex::new(HashMap::new()),
        }
    }
}

impl NdiBackend for RealNdiBackend {
    fn send_create(&self, name: &str) -> Result<usize, NdiError> {
        let c_name = CString::new(name).map_err(|_| NdiError::InitFailed)?;

        let create_desc = NDIlib_send_create_t {
            p_ndi_name: c_name.as_ptr(),
            p_groups: ptr::null(),
            clock_video: false,
            clock_audio: false,
        };

        let ptr = unsafe { (self.lib.send_create)(&create_desc) };
        if ptr.is_null() {
            return Err(NdiError::InitFailed);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(id, ptr);
        info!("Created NDI sender '{name}' with handle {id}");
        Ok(id)
    }

    fn send_destroy(&self, handle: usize) {
        if let Some(ptr) = self.handles.lock().unwrap().remove(&handle) {
            debug!("Destroying NDI sender handle {handle}");
            unsafe {
                (self.lib.send_destroy)(ptr);
            }
        }
    }

    fn send_video(
        &self,
        handle: usize,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
    ) {
        let handles = self.handles.lock().unwrap();
        let Some(&ptr) = handles.get(&handle) else {
            return;
        };

        let frame = NDIlib_video_frame_v2_t {
            xres: width,
            yres: height,
            four_cc: FourCCVideoType::BGRA,
            frame_rate_n,
            frame_rate_d,
            picture_aspect_ratio: 0.0,
            frame_format_type: FRAME_FORMAT_PROGRESSIVE,
            timecode: NDI_SEND_TIMECODE_SYNTHESIZE,
            p_data: data.as_ptr(),
            line_stride_in_bytes: stride,
            p_metadata: ptr::null(),
            timestamp: 0,
        };

        unsafe {
            (self.lib.send_send_video_v2)(ptr, &frame);
        }
    }

    fn send_audio(
        &self,
        handle: usize,
        sample_rate: i32,
        channels: i32,
        samples: i32,
        data: &[f32],
    ) {
        let handles = self.handles.lock().unwrap();
        let Some(&ptr) = handles.get(&handle) else {
            return;
        };

        let frame = NDIlib_audio_frame_v3_t {
            sample_rate,
            no_channels: channels,
            no_samples: samples,
            timecode: NDI_SEND_TIMECODE_SYNTHESIZE,
            four_cc: FourCCAudioType::FLTP,
            p_data: data.as_ptr(),
            channel_stride_in_bytes: 0, // interleaved — no per-channel stride
            p_metadata: ptr::null(),
            timestamp: 0,
        };

        unsafe {
            (self.lib.send_send_audio_v3)(ptr, &frame);
        }
    }

    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
        let handles = self.handles.lock().unwrap();
        let ptr = *handles.get(&handle)?;

        let mut tally = NDIlib_tally_t::default();
        let changed = unsafe { (self.lib.send_get_tally)(ptr, &mut tally, timeout_ms) };
        if changed {
            Some((tally.on_program, tally.on_preview))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// NdiSender — high-level sender that wraps a backend
// ---------------------------------------------------------------------------

/// High-level NDI sender.
///
/// Generic over `B: NdiBackend` so tests can inject a mock.
/// On [`Drop`], the underlying sender instance is destroyed.
pub struct NdiSender<B: NdiBackend> {
    backend: Arc<B>,
    handle: usize,
}

impl<B: NdiBackend> NdiSender<B> {
    /// Create a new NDI sender with the given source name.
    pub fn new(backend: Arc<B>, name: &str) -> Result<Self, NdiError> {
        let handle = backend.send_create(name)?;
        Ok(Self { backend, handle })
    }

    /// Send a video frame.
    pub fn send_video(&self, frame: &VideoFrame) {
        self.backend.send_video(
            self.handle,
            frame.width as i32,
            frame.height as i32,
            frame.stride as i32,
            frame.frame_rate_n,
            frame.frame_rate_d,
            &frame.data,
        );
    }

    /// Send an audio frame.
    pub fn send_audio(&self, frame: &AudioFrame) {
        let samples_per_channel = if frame.channels > 0 {
            frame.data.len() as i32 / frame.channels as i32
        } else {
            0
        };
        self.backend.send_audio(
            self.handle,
            frame.sample_rate as i32,
            frame.channels as i32,
            samples_per_channel,
            &frame.data,
        );
    }

    /// Query the tally state (program / preview) with a timeout in milliseconds.
    pub fn get_tally(&self, timeout_ms: u32) -> Option<Tally> {
        self.backend
            .send_get_tally(self.handle, timeout_ms)
            .map(|(on_program, on_preview)| Tally {
                on_program,
                on_preview,
            })
    }

    /// Return the internal handle ID (useful for tests).
    pub fn handle(&self) -> usize {
        self.handle
    }
}

impl<B: NdiBackend> Drop for NdiSender<B> {
    fn drop(&mut self) {
        self.backend.send_destroy(self.handle);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// A mock backend that records every call for assertion.
    #[derive(Default)]
    struct MockNdiBackend {
        calls: StdMutex<Vec<String>>,
        tally_response: StdMutex<Option<(bool, bool)>>,
    }

    impl MockNdiBackend {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn set_tally(&self, on_program: bool, on_preview: bool) {
            *self.tally_response.lock().unwrap() = Some((on_program, on_preview));
        }
    }

    impl NdiBackend for MockNdiBackend {
        fn send_create(&self, name: &str) -> Result<usize, NdiError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send_create({name})"));
            Ok(42)
        }

        fn send_destroy(&self, handle: usize) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send_destroy({handle})"));
        }

        fn send_video(
            &self,
            handle: usize,
            width: i32,
            height: i32,
            stride: i32,
            frame_rate_n: i32,
            frame_rate_d: i32,
            _data: &[u8],
        ) {
            self.calls.lock().unwrap().push(format!(
                "send_video({handle}, {width}x{height}, stride={stride}, {frame_rate_n}/{frame_rate_d})"
            ));
        }

        fn send_audio(
            &self,
            handle: usize,
            sample_rate: i32,
            channels: i32,
            samples: i32,
            _data: &[f32],
        ) {
            self.calls.lock().unwrap().push(format!(
                "send_audio({handle}, sr={sample_rate}, ch={channels}, smp={samples})"
            ));
        }

        fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send_get_tally({handle}, {timeout_ms})"));
            *self.tally_response.lock().unwrap()
        }
    }

    #[test]
    fn sender_new_calls_backend_create() {
        let backend = Arc::new(MockNdiBackend::default());
        let _sender = NdiSender::new(backend.clone(), "TestSrc").unwrap();
        assert_eq!(backend.calls(), vec!["send_create(TestSrc)"]);
    }

    #[test]
    fn sender_send_video_forwards_dimensions() {
        let backend = Arc::new(MockNdiBackend::default());
        let sender = NdiSender::new(backend.clone(), "V").unwrap();

        let frame = VideoFrame {
            data: vec![0u8; 1920 * 1080 * 4],
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            frame_rate_n: 30000,
            frame_rate_d: 1001,
        };
        sender.send_video(&frame);

        let calls = backend.calls();
        assert_eq!(calls.len(), 2); // create + send_video
        assert_eq!(
            calls[1],
            "send_video(42, 1920x1080, stride=7680, 30000/1001)"
        );
    }

    #[test]
    fn sender_send_audio_computes_samples_per_channel() {
        let backend = Arc::new(MockNdiBackend::default());
        let sender = NdiSender::new(backend.clone(), "A").unwrap();

        // 2 channels, 480 total samples → 240 per channel
        let frame = AudioFrame {
            data: vec![0.0f32; 480],
            channels: 2,
            sample_rate: 48000,
        };
        sender.send_audio(&frame);

        let calls = backend.calls();
        assert_eq!(calls[1], "send_audio(42, sr=48000, ch=2, smp=240)");
    }

    #[test]
    fn sender_drop_calls_destroy() {
        let backend = Arc::new(MockNdiBackend::default());
        {
            let _sender = NdiSender::new(backend.clone(), "D").unwrap();
            // sender dropped here
        }
        let calls = backend.calls();
        assert_eq!(calls.last().unwrap(), "send_destroy(42)");
    }

    #[test]
    fn sender_get_tally_returns_none_when_no_change() {
        let backend = Arc::new(MockNdiBackend::default());
        let sender = NdiSender::new(backend.clone(), "T").unwrap();

        assert!(sender.get_tally(100).is_none());
    }

    #[test]
    fn sender_get_tally_returns_values() {
        let backend = Arc::new(MockNdiBackend::default());
        backend.set_tally(true, false);
        let sender = NdiSender::new(backend.clone(), "T").unwrap();

        let tally = sender.get_tally(100).unwrap();
        assert!(tally.on_program);
        assert!(!tally.on_preview);
    }

    #[test]
    fn sender_get_tally_both_true() {
        let backend = Arc::new(MockNdiBackend::default());
        backend.set_tally(true, true);
        let sender = NdiSender::new(backend.clone(), "T").unwrap();

        let tally = sender.get_tally(0).unwrap();
        assert_eq!(
            tally,
            Tally {
                on_program: true,
                on_preview: true
            }
        );
    }
}
