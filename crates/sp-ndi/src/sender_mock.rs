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
    /// Overrides what `send_get_source_url` returns (#196). `None` (default)
    /// makes the getter return a deterministic synthetic `127.0.0.1:59<hh>`
    /// derived from the handle; a set value is returned verbatim so tests can
    /// assert a specific advertised URL flows through to the health snapshot.
    source_url: StdMutex<Option<String>>,
    /// #196: what `discover_local_sources` returns — the (synthetic) `(name,
    /// url)` pairs a test injects to drive the finder → `match_source_urls`
    /// path on Linux. Empty by default (no NDI runtime).
    discovered_sources: StdMutex<Vec<(String, String)>>,
    /// #203: the `(data.as_ptr() as usize, data.len())` of the LAST
    /// `send_video_async` call. Lets a Linux test prove
    /// `NdiSender::send_video_async_slice` hands the backend the EXACT borrowed
    /// slice (same pointer + length), i.e. no hidden copy / re-slice. `None`
    /// until the first async video send.
    last_async_video_slice: StdMutex<Option<(usize, usize)>>,
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

    /// #203: `(ptr, len)` of the pixel slice the backend received on the LAST
    /// `send_video_async`, or `None` if none happened. Proves a borrowed-slice
    /// send forwards the caller's bytes without a copy or re-slice.
    pub fn last_async_video_slice(&self) -> Option<(usize, usize)> {
        *self.last_async_video_slice.lock().unwrap()
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

    /// Override what `send_get_source_url` returns (#196). Pass `Some(url)` to
    /// assert a specific advertised `host:port` flows through the health path;
    /// leaving it unset yields a deterministic synthetic address.
    pub fn set_source_url(&self, url: Option<String>) {
        *self.source_url.lock().unwrap() = url;
    }

    /// #196: set the (synthetic) sources `discover_local_sources` returns, so a
    /// Linux test can drive the finder → `find::match_source_urls` path.
    pub fn set_discovered_sources(&self, sources: Vec<(String, String)>) {
        *self.discovered_sources.lock().unwrap() = sources;
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
        data: &[u8],
        timecode_100ns: Option<i64>,
    ) {
        self.calls.lock().unwrap().push(format!(
            "send_video_async({handle},{four_cc:?},{width}x{height},stride={stride},{frame_rate_n}/{frame_rate_d})"
        ));
        let tc = timecode_100ns.unwrap_or(NDI_SEND_TIMECODE_SYNTHESIZE);
        self.video_timecodes.lock().unwrap().push(tc);
        // #203: record the exact borrowed slice the backend received.
        *self.last_async_video_slice.lock().unwrap() = Some((data.as_ptr() as usize, data.len()));
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

    fn send_get_source_url(&self, handle: usize) -> Option<String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("send_get_source_url({handle})"));
        match &*self.source_url.lock().unwrap() {
            Some(u) => Some(u.clone()),
            // Deterministic synthetic address (handle 42 → 127.0.0.1:5942) so a
            // Linux test can assert the URL threads through to the snapshot.
            None => Some(format!("127.0.0.1:59{:02}", handle % 100)),
        }
    }

    fn discover_local_sources(
        &self,
        want_names: &[String],
        _overall_timeout_ms: u32,
    ) -> Vec<(String, String)> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("discover_local_sources({})", want_names.len()));
        self.discovered_sources.lock().unwrap().clone()
    }
}
