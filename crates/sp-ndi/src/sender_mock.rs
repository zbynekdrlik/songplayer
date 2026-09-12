//! Mock [`MockNdiBackend`] — records every call for assertion.
//!
//! Split out of `sender.rs` (which reached the airuleset 1000-line file-size
//! cap, #147) into a `real`/`mock` sibling pair. Pure move: no behaviour
//! change. Compiled under `#[cfg(any(test, feature = "test-util"))]`. The
//! backward-compatible path `sp_ndi::test_util::MockNdiBackend` (and the crate
//! root `sp_ndi::MockNdiBackend`) still resolve via the re-exports in
//! `sender.rs` / `lib.rs`.

use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::deinterleave::deinterleave;
use crate::error::NdiError;
use crate::sender::NdiBackend;
use crate::types::{FourCCVideoType, NDI_SEND_TIMECODE_SYNTHESIZE};

/// A mock backend that records every call for assertion.
#[derive(Default)]
pub struct MockNdiBackend {
    calls: StdMutex<Vec<String>>,
    tally_response: StdMutex<Option<(bool, bool)>>,
    last_audio_planar: StdMutex<Vec<f32>>,
    connection_count: AtomicI32,
    /// Resolved video timecodes recorded per `send_video{,_async}` call —
    /// `Some(t) -> t`, `None -> NDI_SEND_TIMECODE_SYNTHESIZE` (exactly what
    /// `RealNdiBackend` writes into the frame). Kept out of the `calls`
    /// strings so existing exact-match assertions stay stable.
    video_timecodes: StdMutex<Vec<i64>>,
    /// Resolved audio timecodes, same convention as `video_timecodes`.
    audio_timecodes: StdMutex<Vec<i64>>,
}

impl MockNdiBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn last_audio_planar(&self) -> Vec<f32> {
        self.last_audio_planar.lock().unwrap().clone()
    }

    /// Resolved video timecodes recorded so far (one per video send).
    pub fn video_timecodes(&self) -> Vec<i64> {
        self.video_timecodes.lock().unwrap().clone()
    }

    /// Resolved audio timecodes recorded so far (one per audio send).
    pub fn audio_timecodes(&self) -> Vec<i64> {
        self.audio_timecodes.lock().unwrap().clone()
    }

    pub fn set_tally(&self, on_program: bool, on_preview: bool) {
        *self.tally_response.lock().unwrap() = Some((on_program, on_preview));
    }

    /// Drive the value `MockNdiBackend::send_get_no_connections` returns.
    /// Lets unit tests exercise every NDI-health alert branch without a
    /// real NDI runtime.
    pub fn set_connection_count(&self, n: i32) {
        self.connection_count.store(n, Ordering::SeqCst);
    }
}

impl NdiBackend for MockNdiBackend {
    fn send_create_with_clocking(
        &self,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<usize, NdiError> {
        self.calls.lock().unwrap().push(format!(
            "send_create_with_clocking({name},{clock_video},{clock_audio})"
        ));
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
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        _data: &[u8],
        timecode_100ns: Option<i64>,
    ) {
        self.calls.lock().unwrap().push(format!(
            "send_video({handle},{four_cc:?},{width}x{height},stride={stride},{frame_rate_n}/{frame_rate_d})"
        ));
        let tc = timecode_100ns.unwrap_or(NDI_SEND_TIMECODE_SYNTHESIZE);
        self.video_timecodes.lock().unwrap().push(tc);
    }

    unsafe fn send_video_async(
        &self,
        handle: usize,
        four_cc: FourCCVideoType,
        width: i32,
        height: i32,
        stride: i32,
        frame_rate_n: i32,
        frame_rate_d: i32,
        _data: &[u8],
        timecode_100ns: Option<i64>,
    ) {
        self.calls.lock().unwrap().push(format!(
            "send_video_async({handle},{four_cc:?},{width}x{height},stride={stride},{frame_rate_n}/{frame_rate_d})"
        ));
        let tc = timecode_100ns.unwrap_or(NDI_SEND_TIMECODE_SYNTHESIZE);
        self.video_timecodes.lock().unwrap().push(tc);
    }

    fn send_video_flush(&self, handle: usize) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("send_video_flush({handle})"));
    }

    fn send_audio(
        &self,
        handle: usize,
        sample_rate: i32,
        channels: i32,
        samples_per_channel: i32,
        interleaved: &[f32],
        timecode_100ns: Option<i64>,
    ) {
        self.calls.lock().unwrap().push(format!(
            "send_audio({handle},sr={sample_rate},ch={channels},spc={samples_per_channel})"
        ));
        let tc = timecode_100ns.unwrap_or(NDI_SEND_TIMECODE_SYNTHESIZE);
        self.audio_timecodes.lock().unwrap().push(tc);
        // Record the planar form for tests that want to verify layout.
        let mut scratch = Vec::new();
        deinterleave(interleaved, channels as usize, &mut scratch);
        *self.last_audio_planar.lock().unwrap() = scratch;
    }

    fn send_get_tally(&self, handle: usize, timeout_ms: u32) -> Option<(bool, bool)> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("send_get_tally({handle},{timeout_ms})"));
        *self.tally_response.lock().unwrap()
    }

    fn send_get_no_connections(&self, handle: usize, timeout_ms: u32) -> i32 {
        self.calls
            .lock()
            .unwrap()
            .push(format!("send_get_no_connections({handle},{timeout_ms})"));
        self.connection_count.load(Ordering::SeqCst)
    }
}
