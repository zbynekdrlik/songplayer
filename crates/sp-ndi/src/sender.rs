//! High-level NDI sender with a mockable backend trait.
//!
//! The concrete backends live in sibling modules to keep this file under the
//! airuleset 1000-line cap (#147): [`RealNdiBackend`] in `sender_real.rs`,
//! `MockNdiBackend` in `sender_mock.rs`. Both are re-exported here so
//! `sp_ndi::sender::RealNdiBackend` / `sp_ndi::test_util::MockNdiBackend`
//! still resolve.

use std::sync::Arc;

use crate::error::NdiError;
use crate::types::{FourCCVideoType, PixelFormat};

pub use crate::sender_real::RealNdiBackend;

// ---------------------------------------------------------------------------
// Safe public frame types
// ---------------------------------------------------------------------------

/// A video frame ready to send over NDI.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Raw pixel data in the layout required by `pixel_format`.
    pub data: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per scan line. For BGRA: `width * 4`. For NV12: Y-plane row bytes
    /// (usually `width`).
    pub stride: u32,
    /// Frame rate numerator (e.g. 30 for 30 fps, 30000 for 29.97).
    pub frame_rate_n: i32,
    /// Frame rate denominator (e.g. 1 for 30 fps, 1001 for 29.97).
    pub frame_rate_d: i32,
    /// Pixel format. Determines the FourCC sent to NDI and the stride semantic.
    pub pixel_format: PixelFormat,
    /// Genlock video timecode: `floor_boundary_100ns(present_wall)` in 100-ns
    /// units since the Unix epoch (camera-box#1294 §4). `None` keeps the NDI
    /// SYNTHESIZE marker — used only for the standby black frame, which is not
    /// on the grid yet (camera-box#1294 open question 7).
    pub timecode_100ns: Option<i64>,
}

/// An audio frame ready to send over NDI. Data is interleaved f32 PCM — the
/// sender converts to planar FLTP internally.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Interleaved float samples `[c0_s0, c1_s0, …, c0_s1, c1_s1, …]`.
    pub data: Vec<f32>,
    /// Number of audio channels.
    pub channels: u32,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Genlock audio timecode: the raw wall clock at submission in 100-ns
    /// units since the Unix epoch, with NO boundary snap (camera-box#1294 §6).
    /// `None` keeps the NDI SYNTHESIZE marker.
    pub timecode_100ns: Option<i64>,
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
    /// Create a sender with explicit `clock_video` / `clock_audio` flags.
    fn send_create_with_clocking(
        &self,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<usize, NdiError>;

    /// Destroy a sender instance.
    fn send_destroy(&self, handle: usize);

    /// Send a video frame synchronously (blocks if the sender was created with
    /// `clock_video = true`).
    #[allow(clippy::too_many_arguments)]
    fn send_video(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
        timecode_100ns: Option<i64>,
    );

    /// Schedule a video frame for asynchronous send.
    ///
    /// # Safety
    ///
    /// The caller must keep `data` valid until the next synchronising call on
    /// this sender — another `send_video_async`, a `send_video`,
    /// `send_video_flush`, or `send_destroy`. If the buffer is freed before
    /// that point, the NDI SDK will dereference freed memory (UB).
    #[allow(clippy::too_many_arguments)]
    unsafe fn send_video_async(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        data: &[u8],
        timecode_100ns: Option<i64>,
    );

    /// Flush the last async frame by calling `send_send_video_async_v2(NULL)`.
    /// After this returns, the previous frame's buffer may be freed.
    fn send_video_flush(&self, handle: usize);

    /// Send an audio frame. The backend is responsible for converting the
    /// interleaved float input into the planar FLTP layout NDI requires.
    fn send_audio(
        &self,
        handle: usize,
        sample_rate: i32,
        channels: i32,
        samples_per_channel: i32,
        interleaved: &[f32],
        timecode_100ns: Option<i64>,
    );

    /// Query tally state. Returns `None` if the timeout expired with no change.
    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)>;

    /// Return the current number of NDI receivers connected to this sender.
    /// Returns `>= 0` when the SDK reports a count; the caller must treat any
    /// negative value as "unknown" and not as a failure (the NDI SDK may
    /// occasionally use negatives to mean "never been polled").
    ///
    /// `timeout_ms = 0` is the recommended value: the SDK returns the cached
    /// count immediately. With `> 0` the call blocks until the count changes
    /// or the timeout expires.
    fn send_get_no_connections(&self, handle: usize, timeout_ms: u32) -> i32;
}

// ---------------------------------------------------------------------------
// NdiSender — high-level sender that wraps a backend
// ---------------------------------------------------------------------------

/// High-level NDI sender.
///
/// Generic over `B: NdiBackend` so tests can inject a mock.
/// On [`Drop`], the sender flushes any pending async frame and destroys the
/// underlying NDI instance.
pub struct NdiSender<B: NdiBackend> {
    backend: Arc<B>,
    handle: usize,
}

impl<B: NdiBackend> NdiSender<B> {
    /// Create a sender with explicit clocking flags.
    ///
    /// Genlock note (#146/#147): the genlock path drives cadence from the
    /// wall-clock timecode carried on every real frame, not from the SDK
    /// clock, and drops the SYNTHESIZE marker for real sends. `clock_video`
    /// SDK pacing is retained as an interim measure and is replaced by
    /// boundary-paced emission in #147; it is no longer "the recommended
    /// configuration".
    ///
    /// Do NOT set both `clock_video` and `clock_audio` to `true` from a
    /// single submission thread: each clocked send blocks until the wall clock
    /// reaches the frame's natural time, and the two streams would dead-clock
    /// each other. Use both-true only when video and audio are submitted from
    /// separate threads.
    pub fn new_with_clocking(
        backend: Arc<B>,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<Self, NdiError> {
        let handle = backend.send_create_with_clocking(name, clock_video, clock_audio)?;
        Ok(Self { backend, handle })
    }

    /// Send a video frame synchronously.
    pub fn send_video(&self, frame: &VideoFrame) {
        let four_cc = match frame.pixel_format {
            PixelFormat::Bgra => FourCCVideoType::BGRA,
            PixelFormat::Nv12 => FourCCVideoType::NV12,
        };
        self.backend.send_video(
            self.handle,
            four_cc,
            frame.width as i32,
            frame.height as i32,
            frame.stride as i32,
            frame.frame_rate_n,
            frame.frame_rate_d,
            &frame.data,
            frame.timecode_100ns,
        );
    }

    /// Schedule a video frame for async send.
    ///
    /// # Safety
    ///
    /// The caller must keep `frame.data` alive until the next synchronising
    /// call on this sender — another `send_video_async`, a `send_video`,
    /// `send_video_flush`, or when the sender is dropped. If the buffer is
    /// freed before that point, the NDI SDK will dereference freed memory (UB).
    pub unsafe fn send_video_async(&self, frame: &VideoFrame) {
        let four_cc = match frame.pixel_format {
            PixelFormat::Bgra => FourCCVideoType::BGRA,
            PixelFormat::Nv12 => FourCCVideoType::NV12,
        };
        unsafe {
            self.backend.send_video_async(
                self.handle,
                four_cc,
                frame.width as i32,
                frame.height as i32,
                frame.stride as i32,
                frame.frame_rate_n,
                frame.frame_rate_d,
                &frame.data,
                frame.timecode_100ns,
            );
        }
    }

    /// Release any pending async frame. Must be called before dropping the
    /// buffer of the last async frame.
    pub fn send_video_flush(&self) {
        self.backend.send_video_flush(self.handle);
    }

    /// Send an audio frame.
    pub fn send_audio(&self, frame: &AudioFrame) {
        if frame.channels == 0 {
            return;
        }
        let samples_per_channel = frame.data.len() as i32 / frame.channels as i32;
        self.backend.send_audio(
            self.handle,
            frame.sample_rate as i32,
            frame.channels as i32,
            samples_per_channel,
            &frame.data,
            frame.timecode_100ns,
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

    /// Return the current count of NDI receivers connected to this sender.
    /// `timeout_ms = 0` returns immediately with the SDK's cached count.
    pub fn get_no_connections(&self, timeout_ms: u32) -> i32 {
        self.backend
            .send_get_no_connections(self.handle, timeout_ms)
    }

    /// Return the internal handle ID (useful for tests).
    pub fn handle(&self) -> usize {
        self.handle
    }

    /// A cheap, cloneable audio-only send handle over this sender's backend +
    /// handle. NDI permits audio and video to be submitted from SEPARATE threads
    /// on the same sender instance, so a dedicated wall-clock audio emitter
    /// thread (#192, `sp_server::playback::audio_emitter`) can call
    /// [`AudioSink::send_audio`] while the decode thread submits video through
    /// the [`FrameSubmitter`], without sharing `&mut FrameSubmitter` across
    /// threads. The sink holds an `Arc<B>` clone of the backend, so the backend
    /// outlives it; the OWNER of the sink MUST be torn down before this
    /// `NdiSender` is dropped (`send_destroy` invalidates the handle), which the
    /// emitter-thread lifecycle guarantees (spawned at sender create, joined
    /// before the submitter — and thus the sender — drops).
    pub fn audio_sink(&self) -> AudioSink<B> {
        AudioSink {
            backend: self.backend.clone(),
            handle: self.handle,
        }
    }
}

/// A cloneable audio-only send handle (see [`NdiSender::audio_sink`]). Sends
/// interleaved-f32 audio frames on the same NDI sender instance from a
/// different thread than the video submit path.
#[derive(Clone)]
pub struct AudioSink<B: NdiBackend> {
    backend: Arc<B>,
    handle: usize,
}

impl<B: NdiBackend> AudioSink<B> {
    /// Send one interleaved-f32 audio frame. Mirrors [`NdiSender::send_audio`]:
    /// a zero-channel frame is a no-op; the backend planarises to FLTP.
    pub fn send_audio(&self, frame: &AudioFrame) {
        if frame.channels == 0 {
            return;
        }
        let samples_per_channel = frame.data.len() as i32 / frame.channels as i32;
        self.backend.send_audio(
            self.handle,
            frame.sample_rate as i32,
            frame.channels as i32,
            samples_per_channel,
            &frame.data,
            frame.timecode_100ns,
        );
    }
}

impl<B: NdiBackend> Drop for NdiSender<B> {
    fn drop(&mut self) {
        // Flush any pending async frame before destroying — guarantees the SDK
        // has released its pointer to our last buffer.
        self.backend.send_video_flush(self.handle);
        self.backend.send_destroy(self.handle);
    }
}

// ---------------------------------------------------------------------------
// Mock backend — re-exported from the `sender_mock` sibling module.
// ---------------------------------------------------------------------------

/// Backward-compatible re-export: the mock backend now lives in
/// `sender_mock.rs` (split out for the airuleset file-size cap, #147).
/// External code keeps addressing it as `sp_ndi::test_util::MockNdiBackend`.
#[cfg(any(test, feature = "test-util"))]
pub mod test_util {
    pub use crate::sender_mock::MockNdiBackend;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use test_util::MockNdiBackend;

    #[test]
    fn new_with_clocking_defaults_false_false() {
        let backend = Arc::new(MockNdiBackend::new());
        let _s = NdiSender::new_with_clocking(backend.clone(), "X", false, false).unwrap();
        let calls = backend.calls();
        assert_eq!(calls[0], "send_create_with_clocking(X,false,false)");
    }

    #[test]
    fn new_with_clocking_forwards_flags() {
        let backend = Arc::new(MockNdiBackend::new());
        let _s = NdiSender::new_with_clocking(backend.clone(), "Y", true, false).unwrap();
        let calls = backend.calls();
        assert_eq!(calls[0], "send_create_with_clocking(Y,true,false)");
    }

    #[test]
    fn send_video_async_records_nv12_fourcc_and_size() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "V", false, false).unwrap();

        let frame = VideoFrame {
            data: vec![0u8; 1920 * 1080 * 3 / 2],
            width: 1920,
            height: 1080,
            stride: 1920,
            frame_rate_n: 30,
            frame_rate_d: 1,
            pixel_format: PixelFormat::Nv12,
            timecode_100ns: None,
        };
        // SAFETY: `frame` outlives this call and a flush happens on drop.
        unsafe { sender.send_video_async(&frame) };
        let calls = backend.calls();
        assert_eq!(
            calls[1],
            "send_video_async(42,NV12,1920x1080,stride=1920,30/1)"
        );
    }

    #[test]
    fn async_round_trip_records_async_async_flush_in_order() {
        // This exercises the double-buffer pattern Task 10's FrameSubmitter relies
        // on: two async sends in sequence (the second acts as the sync point that
        // releases the first's buffer), followed by an explicit flush that releases
        // the second's buffer. The mock records all three calls in the exact order.
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "R", false, false).unwrap();

        let frame_a = VideoFrame {
            data: vec![0u8; 4 * 2 * 3 / 2],
            width: 4,
            height: 2,
            stride: 4,
            frame_rate_n: 30,
            frame_rate_d: 1,
            pixel_format: PixelFormat::Nv12,
            timecode_100ns: None,
        };
        let frame_b = VideoFrame {
            data: vec![0u8; 4 * 2 * 3 / 2],
            width: 4,
            height: 2,
            stride: 4,
            frame_rate_n: 30,
            frame_rate_d: 1,
            pixel_format: PixelFormat::Nv12,
            timecode_100ns: None,
        };

        // SAFETY: the buffers in `frame_a` and `frame_b` outlive every call below,
        // and a flush releases the SDK's last retained pointer before they drop.
        unsafe {
            sender.send_video_async(&frame_a);
            sender.send_video_async(&frame_b);
        }
        sender.send_video_flush();

        let calls = backend.calls();
        // create, async_a, async_b, flush — skipping the later drop flush+destroy
        assert_eq!(calls[0], "send_create_with_clocking(R,false,false)");
        assert_eq!(calls[1], "send_video_async(42,NV12,4x2,stride=4,30/1)");
        assert_eq!(calls[2], "send_video_async(42,NV12,4x2,stride=4,30/1)");
        assert_eq!(calls[3], "send_video_flush(42)");
    }

    #[test]
    fn send_video_records_bgra_fourcc() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "B", false, false).unwrap();
        let frame = VideoFrame {
            data: vec![0u8; 4],
            width: 1,
            height: 1,
            stride: 4,
            frame_rate_n: 30,
            frame_rate_d: 1,
            pixel_format: PixelFormat::Bgra,
            timecode_100ns: None,
        };
        sender.send_video(&frame);
        let calls = backend.calls();
        assert_eq!(calls[1], "send_video(42,BGRA,1x1,stride=4,30/1)");
    }

    #[test]
    fn send_audio_records_samples_and_planarises() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "A", false, false).unwrap();

        // 2ch interleaved, 4 samples/ch → 8 floats
        let frame = AudioFrame {
            data: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0],
            channels: 2,
            sample_rate: 48000,
            timecode_100ns: None,
        };
        sender.send_audio(&frame);

        let calls = backend.calls();
        assert_eq!(calls[1], "send_audio(42,sr=48000,ch=2,spc=4)");

        // Planar: [1,3,5,7, 2,4,6,8]
        assert_eq!(
            backend.last_audio_planar(),
            vec![1.0, 3.0, 5.0, 7.0, 2.0, 4.0, 6.0, 8.0]
        );
    }

    #[test]
    fn send_video_flush_is_recorded() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "F", false, false).unwrap();
        sender.send_video_flush();
        let calls = backend.calls();
        assert_eq!(calls.last().unwrap(), "send_video_flush(42)");
    }

    #[test]
    fn dropping_sender_calls_destroy_before_subsequent_create() {
        // Drop semantics check: dropping an `NdiSender` calls
        // `send_video_flush + send_destroy` on the underlying backend before
        // anything else can run. Useful baseline for any future redesign
        // that wants to recreate a sender (#60 NDI runtime re-init).
        //
        // Note: real NDI runtime rejects two senders with the same name
        // in one process; `MockNdiBackend` does not enforce that. The
        // 2026-04-27 production failure (PR #58 RecreateSender loop) was
        // caused exactly by code that called `send_create` for the new
        // sender BEFORE the old sender's `send_destroy` ran — `MockNdiBackend`
        // happily accepted both creates while the real SDK returned null.
        // Real-NDI integration testing is the only way to catch that;
        // this test only verifies the drop ordering inside our wrapper.
        let backend = std::sync::Arc::new(MockNdiBackend::new());
        {
            let s = NdiSender::new_with_clocking(backend.clone(), "RX", true, false).unwrap();
            // Drop happens at end of scope.
            drop(s);
        }
        let _s2 = NdiSender::new_with_clocking(backend.clone(), "RX", true, false).unwrap();

        let calls = backend.calls();
        let first_create = calls
            .iter()
            .position(|c| c.starts_with("send_create_with_clocking"))
            .expect("first sender must call send_create");
        let destroy = calls
            .iter()
            .position(|c| c.starts_with("send_destroy"))
            .expect("dropping the first sender must call send_destroy");
        let second_create = calls
            .iter()
            .rposition(|c| c.starts_with("send_create_with_clocking"))
            .expect("second sender must call send_create");

        assert!(
            first_create < destroy && destroy < second_create,
            "expected create -> destroy -> create ordering, got {calls:#?}"
        );
    }

    #[test]
    fn sender_drop_flushes_then_destroys() {
        let backend = Arc::new(MockNdiBackend::new());
        {
            let _s = NdiSender::new_with_clocking(backend.clone(), "D", false, false).unwrap();
        }
        let calls = backend.calls();
        // create, then on drop: flush, then destroy
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0], "send_create_with_clocking(D,false,false)");
        assert_eq!(calls[1], "send_video_flush(42)");
        assert_eq!(calls[2], "send_destroy(42)");
    }

    #[test]
    fn send_audio_zero_channels_is_noop() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Z", false, false).unwrap();
        let frame = AudioFrame {
            data: vec![1.0, 2.0],
            channels: 0,
            sample_rate: 48000,
            timecode_100ns: None,
        };
        sender.send_audio(&frame);
        // Only create + drop-flush + destroy — no send_audio recorded.
        let calls = backend.calls();
        assert!(calls.iter().all(|c| !c.starts_with("send_audio")));
    }

    #[test]
    fn get_tally_returns_recorded_value() {
        let backend = Arc::new(MockNdiBackend::new());
        backend.set_tally(true, false);
        let sender = NdiSender::new_with_clocking(backend.clone(), "T", false, false).unwrap();
        let tally = sender.get_tally(100).unwrap();
        assert!(tally.on_program);
        assert!(!tally.on_preview);
    }

    #[test]
    fn get_tally_returns_none_by_default() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "T", false, false).unwrap();
        assert!(sender.get_tally(0).is_none());
    }

    #[test]
    fn mock_get_no_connections_returns_set_count() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "C", true, false).unwrap();
        // Default before any setter: 0 (no receivers).
        assert_eq!(sender.get_no_connections(0), 0);
        backend.set_connection_count(3);
        assert_eq!(sender.get_no_connections(0), 3);
        backend.set_connection_count(0);
        assert_eq!(sender.get_no_connections(0), 0);
    }

    #[test]
    fn mock_get_no_connections_records_call() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "C2", true, false).unwrap();
        let _ = sender.get_no_connections(50);
        let calls = backend.calls();
        assert!(
            calls.iter().any(|c| c == "send_get_no_connections(42,50)"),
            "expected send_get_no_connections(handle=42, timeout=50) recorded: {calls:#?}"
        );
    }

    #[test]
    fn mock_set_connection_count_is_thread_safe_via_atomic() {
        // Driven from another thread to confirm visibility — same pattern the
        // pipeline thread will use (heartbeat polls from one thread, the test
        // helper sets from another).
        let backend = Arc::new(MockNdiBackend::new());
        let backend2 = backend.clone();
        let h = std::thread::spawn(move || backend2.set_connection_count(7));
        h.join().unwrap();
        let sender = NdiSender::new_with_clocking(backend, "C3", true, false).unwrap();
        assert_eq!(sender.get_no_connections(0), 7);
    }
}

#[cfg(test)]
#[path = "sender_tests_timecode.rs"]
mod sender_tests_timecode;
