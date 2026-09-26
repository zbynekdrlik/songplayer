//! #212: [`MockNdiReceiveBackend`] — a scripted receive backend for Linux tests.
//!
//! Compiled under `#[cfg(any(test, feature = "test-util"))]` and exported as
//! `sp_ndi::test_util::MockNdiReceiveBackend`. A test hands it a list of
//! video frames plus a SCHEDULE (which frame each successive
//! `framesync_capture_video` returns — the same index twice = a FrameSync
//! repeat, a skipped index = a drop, `None` = the all-zero "no video yet"
//! frame), one planar audio block, and a connection count. It records every
//! create / destroy / capture / free, so a test can prove the RAII wrapper
//! frees each capture and tears down in SDK order (FrameSync, then receiver).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicUsize, Ordering};

use crate::error::NdiError;
use crate::receive::{NDIlib_audio_frame_v2_t, NDIlib_video_frame_v2_recv_t, NdiReceiveBackend};

/// One scripted received video frame (its bytes stay owned by the mock, so the
/// pointer a capture returns is stable until the frames are replaced).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MockVideoFrame {
    pub xres: i32,
    pub yres: i32,
    pub four_cc: u32,
    pub line_stride: i32,
    pub frame_rate_n: i32,
    pub frame_rate_d: i32,
    /// `NDIlib_frame_format_type_e` (1 = progressive).
    pub frame_format_type: i32,
    pub timecode: i64,
    pub data: Vec<u8>,
}

/// The planar audio every capture returns.
#[derive(Default)]
struct MockAudio {
    planar: Vec<f32>,
    channels: i32,
    samples: i32,
    stride_bytes: i32,
}

/// A scripted [`NdiReceiveBackend`] (see the module doc).
#[derive(Default)]
pub struct MockNdiReceiveBackend {
    calls: Mutex<Vec<String>>,
    sources: Mutex<Vec<String>>,
    connections: AtomicI32,
    queue_depth: AtomicI32,
    fail_recv_create: AtomicBool,
    fail_framesync_create: AtomicBool,
    last_handle: AtomicUsize,
    frames: Mutex<Vec<MockVideoFrame>>,
    schedule: Mutex<Vec<Option<usize>>>,
    video_captures: AtomicUsize,
    /// Captured minus freed video frames.
    outstanding_video: AtomicI64,
    audio: Mutex<MockAudio>,
    /// Captured minus freed audio frames.
    outstanding_audio: AtomicI64,
}

impl MockNdiReceiveBackend {
    fn log(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    /// Every recorded call (connection / queue-depth polls are not recorded).
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// The names `find_source_names` returns.
    pub fn set_sources(&self, names: Vec<String>) {
        *self.sources.lock().unwrap() = names;
    }

    /// What `recv_connections` returns (0 = disconnected).
    pub fn set_connections(&self, n: i32) {
        self.connections.store(n, Ordering::SeqCst);
    }

    /// What `framesync_audio_queue_depth` returns.
    pub fn set_audio_queue_depth(&self, n: i32) {
        self.queue_depth.store(n, Ordering::SeqCst);
    }

    /// Make `recv_create` / `framesync_create` fail.
    pub fn set_fail_create(&self, recv: bool, framesync: bool) {
        self.fail_recv_create.store(recv, Ordering::SeqCst);
        self.fail_framesync_create
            .store(framesync, Ordering::SeqCst);
    }

    /// The frames a schedule indexes into.
    pub fn set_video_frames(&self, frames: Vec<MockVideoFrame>) {
        *self.frames.lock().unwrap() = frames;
    }

    /// Capture call `k` returns `schedule[min(k, len - 1)]` (`None` = the
    /// all-zero frame); an empty schedule always returns the all-zero frame.
    pub fn set_video_schedule(&self, schedule: Vec<Option<usize>>) {
        *self.schedule.lock().unwrap() = schedule;
    }

    /// The planar block every audio capture returns: `channels` planes of
    /// `samples` floats, `stride_bytes` apart.
    pub fn set_audio(&self, planar: Vec<f32>, channels: i32, samples: i32, stride_bytes: i32) {
        *self.audio.lock().unwrap() = MockAudio {
            planar,
            channels,
            samples,
            stride_bytes,
        };
    }

    /// Video frames captured and not freed yet.
    pub fn outstanding_video(&self) -> i64 {
        self.outstanding_video.load(Ordering::SeqCst)
    }

    /// Audio frames captured and not freed yet.
    pub fn outstanding_audio(&self) -> i64 {
        self.outstanding_audio.load(Ordering::SeqCst)
    }

    fn next_handle(&self) -> usize {
        self.last_handle.fetch_add(1, Ordering::SeqCst) + 1
    }
}

impl NdiReceiveBackend for MockNdiReceiveBackend {
    fn find_source_names(&self, wait_ms: u32) -> Vec<String> {
        self.log(format!("find_source_names({wait_ms})"));
        self.sources.lock().unwrap().clone()
    }

    fn recv_create(&self, source_name: &str, recv_name: &str) -> Result<usize, NdiError> {
        self.log(format!("recv_create({source_name},{recv_name})"));
        if self.fail_recv_create.load(Ordering::SeqCst) {
            return Err(NdiError::ReceiveFailed("mock recv_create"));
        }
        Ok(self.next_handle())
    }

    fn recv_destroy(&self, recv: usize) {
        self.log(format!("recv_destroy({recv})"));
    }

    fn recv_connections(&self, _recv: usize) -> i32 {
        self.connections.load(Ordering::SeqCst)
    }

    fn framesync_create(&self, recv: usize) -> Result<usize, NdiError> {
        self.log(format!("framesync_create({recv})"));
        if self.fail_framesync_create.load(Ordering::SeqCst) {
            return Err(NdiError::ReceiveFailed("mock framesync_create"));
        }
        Ok(self.next_handle())
    }

    fn framesync_destroy(&self, fs: usize) {
        self.log(format!("framesync_destroy({fs})"));
    }

    fn framesync_capture_video(&self, fs: usize) -> NDIlib_video_frame_v2_recv_t {
        self.log(format!("framesync_capture_video({fs})"));
        self.outstanding_video.fetch_add(1, Ordering::SeqCst);
        let k = self.video_captures.fetch_add(1, Ordering::SeqCst);
        let schedule = self.schedule.lock().unwrap();
        let pick = schedule.get(k).or(schedule.last()).copied().flatten();
        let mut frames = self.frames.lock().unwrap();
        match pick.and_then(|i| frames.get_mut(i)) {
            Some(f) => NDIlib_video_frame_v2_recv_t {
                xres: f.xres,
                yres: f.yres,
                four_cc: f.four_cc,
                frame_rate_n: f.frame_rate_n,
                frame_rate_d: f.frame_rate_d,
                picture_aspect_ratio: 0.0,
                frame_format_type: f.frame_format_type,
                timecode: f.timecode,
                p_data: f.data.as_mut_ptr(),
                line_stride_in_bytes: f.line_stride,
                p_metadata: std::ptr::null(),
                timestamp: f.timecode,
            },
            None => NDIlib_video_frame_v2_recv_t::empty(),
        }
    }

    fn framesync_free_video(&self, fs: usize, _frame: &mut NDIlib_video_frame_v2_recv_t) {
        self.log(format!("framesync_free_video({fs})"));
        self.outstanding_video.fetch_sub(1, Ordering::SeqCst);
    }

    fn framesync_capture_audio(
        &self,
        fs: usize,
        sample_rate: i32,
        channels: i32,
        samples: i32,
    ) -> NDIlib_audio_frame_v2_t {
        self.log(format!(
            "framesync_capture_audio({fs},{sample_rate},{channels},{samples})"
        ));
        self.outstanding_audio.fetch_add(1, Ordering::SeqCst);
        let mut a = self.audio.lock().unwrap();
        if a.planar.is_empty() {
            return NDIlib_audio_frame_v2_t::empty();
        }
        NDIlib_audio_frame_v2_t {
            sample_rate,
            no_channels: a.channels,
            no_samples: a.samples,
            timecode: 0,
            p_data: a.planar.as_mut_ptr(),
            channel_stride_in_bytes: a.stride_bytes,
            p_metadata: std::ptr::null(),
            timestamp: 0,
        }
    }

    fn framesync_free_audio(&self, fs: usize, _frame: &mut NDIlib_audio_frame_v2_t) {
        self.log(format!("framesync_free_audio({fs})"));
        self.outstanding_audio.fetch_sub(1, Ordering::SeqCst);
    }

    fn framesync_audio_queue_depth(&self, _fs: usize) -> i32 {
        self.queue_depth.load(Ordering::SeqCst)
    }
}
