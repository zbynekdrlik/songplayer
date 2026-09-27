//! #212: [`NdiFrameSync`] — a safe NDI receiver + FrameSync pair with RAII.
//!
//! [`NdiFrameSync::connect`] creates the receiver for one source and binds a
//! FrameSync to it; dropping it destroys the FrameSync FIRST, then the receiver
//! (the SDK's documented order: "always destroy the receiver after the
//! frame-sync has been destroyed"). Each capture returns a guard
//! ([`CapturedVideo`] / [`CapturedAudio`]) that borrows the pair and frees the
//! SDK frame on drop, so a frame can never outlive its FrameSync and is never
//! leaked or double-freed.

use std::sync::Arc;

use crate::error::NdiError;
use crate::receive::{NDIlib_audio_frame_v2_t, NDIlib_video_frame_v2_recv_t, NdiReceiveBackend};

/// A connected receiver + its FrameSync (see the module doc).
pub struct NdiFrameSync {
    backend: Arc<dyn NdiReceiveBackend>,
    recv: usize,
    fs: usize,
    source: String,
}

impl NdiFrameSync {
    /// Create a receiver for the NDI source `source` (`"MACHINE (stream)"`),
    /// advertised as `recv_name`, and bind a FrameSync to it. A FrameSync
    /// failure destroys the receiver it was created for.
    pub fn connect(
        backend: Arc<dyn NdiReceiveBackend>,
        source: &str,
        recv_name: &str,
    ) -> Result<Self, NdiError> {
        let recv = backend.recv_create(source, recv_name)?;
        match backend.framesync_create(recv) {
            Ok(fs) => Ok(Self {
                backend,
                recv,
                fs,
                source: source.to_string(),
            }),
            Err(e) => {
                backend.recv_destroy(recv);
                Err(e)
            }
        }
    }

    /// The source this pair receives.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Receiver connections (1 while the source is connected).
    pub fn connections(&self) -> i32 {
        self.backend.recv_connections(self.recv)
    }

    /// Audio samples waiting in the FrameSync.
    pub fn audio_queue_depth(&self) -> i32 {
        self.backend.framesync_audio_queue_depth(self.fs)
    }

    /// The FrameSync's current video frame (progressive), freed when the
    /// guard drops.
    pub fn capture_video(&self) -> CapturedVideo<'_> {
        CapturedVideo {
            frame: self.backend.framesync_capture_video(self.fs),
            sync: self,
        }
    }

    /// Exactly `samples` samples per channel of `channels`-channel audio at
    /// `sample_rate` (the FrameSync resamples to the caller's cadence and
    /// returns silence when the source has none), freed when the guard drops.
    pub fn capture_audio(
        &self,
        sample_rate: i32,
        channels: i32,
        samples: i32,
    ) -> CapturedAudio<'_> {
        CapturedAudio {
            frame: self
                .backend
                .framesync_capture_audio(self.fs, sample_rate, channels, samples),
            sync: self,
        }
    }
}

impl Drop for NdiFrameSync {
    fn drop(&mut self) {
        self.backend.framesync_destroy(self.fs);
        self.backend.recv_destroy(self.recv);
    }
}

/// A captured video frame, freed on drop.
pub struct CapturedVideo<'a> {
    sync: &'a NdiFrameSync,
    frame: NDIlib_video_frame_v2_recv_t,
}

impl CapturedVideo<'_> {
    /// The SDK's frame descriptor (size, FourCC, rate, timecode, pointer).
    pub fn frame(&self) -> &NDIlib_video_frame_v2_recv_t {
        &self.frame
    }

    /// The frame's FIRST plane (`line_stride_in_bytes × yres` bytes — the whole
    /// picture for UYVY, the UYVY plane of a UYVA frame), or `None` for the
    /// all-zero "no video yet" frame or a degenerate descriptor.
    pub fn first_plane(&self) -> Option<&[u8]> {
        let f = &self.frame;
        if f.p_data.is_null() || f.xres <= 0 || f.yres <= 0 || f.line_stride_in_bytes <= 0 {
            return None;
        }
        let len = f.line_stride_in_bytes as usize * f.yres as usize;
        // SAFETY: the SDK owns `len` readable bytes at `p_data` (at least the
        // first plane) until this guard frees the frame; the returned borrow
        // cannot outlive the guard.
        Some(unsafe { std::slice::from_raw_parts(f.p_data, len) })
    }
}

impl Drop for CapturedVideo<'_> {
    fn drop(&mut self) {
        self.sync
            .backend
            .framesync_free_video(self.sync.fs, &mut self.frame);
    }
}

/// A captured audio frame, freed on drop.
pub struct CapturedAudio<'a> {
    sync: &'a NdiFrameSync,
    frame: NDIlib_audio_frame_v2_t,
}

impl CapturedAudio<'_> {
    /// The SDK's frame descriptor.
    pub fn frame(&self) -> &NDIlib_audio_frame_v2_t {
        &self.frame
    }

    /// The samples interleaved into exactly `channels × samples` floats
    /// (missing channels / samples are silence, extra ones are dropped).
    pub fn interleaved(&self, channels: usize, samples: usize) -> Vec<f32> {
        let f = &self.frame;
        let (Ok(src_channels), Ok(stride_bytes), Ok(src_samples)) = (
            usize::try_from(f.no_channels),
            usize::try_from(f.channel_stride_in_bytes),
            usize::try_from(f.no_samples),
        ) else {
            return vec![0.0; channels * samples]; // a negative count: nothing to read
        };
        if f.p_data.is_null() {
            return vec![0.0; channels * samples];
        }
        let plane_stride = stride_bytes / std::mem::size_of::<f32>();
        // SAFETY: an FLTP frame holds `no_channels` planes, each
        // `channel_stride_in_bytes` apart, owned by the SDK until this guard
        // frees it; the slice does not outlive this call.
        let planar = unsafe { std::slice::from_raw_parts(f.p_data, plane_stride * src_channels) };
        interleave_planar(
            planar,
            plane_stride,
            src_channels,
            src_samples,
            channels,
            samples,
        )
    }
}

impl Drop for CapturedAudio<'_> {
    fn drop(&mut self) {
        self.sync
            .backend
            .framesync_free_audio(self.sync.fs, &mut self.frame);
    }
}

/// Interleave planar float audio (`src_channels` planes, each `plane_stride`
/// floats apart, `src_samples` valid samples per plane) into exactly
/// `channels × samples` floats: channel `c`, sample `s` lands at
/// `s × channels + c`; whatever the source lacks is 0.0.
pub fn interleave_planar(
    planar: &[f32],
    plane_stride: usize,
    src_channels: usize,
    src_samples: usize,
    channels: usize,
    samples: usize,
) -> Vec<f32> {
    let mut out = vec![0.0; channels * samples];
    let n = src_samples.min(samples).min(plane_stride);
    for c in 0..src_channels.min(channels) {
        let start = c * plane_stride;
        if let Some(plane) = planar.get(start..start + n) {
            for (s, &v) in plane.iter().enumerate() {
                out[s * channels + c] = v;
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "receiver_tests.rs"]
mod tests;
