//! Frame submission helper for the playback pipeline.
//!
//! Owns an `NdiSender` and enforces the rules required for correct NDI output:
//!
//! 1. For each synced tuple, audio chunks are submitted BEFORE the video frame.
//!    This keeps audio buffered in NDI's internal queue when `clock_video=true`
//!    blocks the calling thread for frame pacing.
//!
//! 2. The previous video frame's `Vec<u8>` buffer is kept alive until the next
//!    `submit` or `flush` call. `NDIlib_send_send_video_async_v2` retains a
//!    pointer to our bytes and only releases it when the next async/sync/flush
//!    call arrives.
//!
//! 3. `flush` is called on every playback exit path (Ended / Stopped /
//!    Shutdown / NewPlay / Error / Pause). Flush itself is a sync point that
//!    releases the previous frame, after which the buffer may be dropped.

use sp_ndi::{AudioFrame, NdiBackend, NdiSender, PixelFormat, VideoFrame};

/// Owns an `NdiSender` plus the previous frame's buffer for the async
/// double-buffer pattern.
pub struct FrameSubmitter<B: NdiBackend> {
    sender: NdiSender<B>,
    /// Keeps the previous async frame's `Vec<u8>` alive until NDI releases
    /// its pointer (which happens when the next submit / flush call fires).
    prev_frame: Option<Vec<u8>>,
    frame_rate_n: i32,
    frame_rate_d: i32,
}

impl<B: NdiBackend> FrameSubmitter<B> {
    /// Create a submitter owning an already-constructed sender.
    pub fn new(sender: NdiSender<B>, frame_rate_n: i32, frame_rate_d: i32) -> Self {
        Self {
            sender,
            prev_frame: None,
            frame_rate_n,
            frame_rate_d,
        }
    }

    /// Update the frame rate used for subsequent submissions. Call this when
    /// a new file is opened and its real frame rate is known.
    pub fn set_frame_rate(&mut self, num: i32, den: i32) {
        self.frame_rate_n = num;
        self.frame_rate_d = den;
    }

    /// Submit one decoded frame tuple: all audio chunks first, then video
    /// asynchronously. Video buffer ownership transfers to the submitter for
    /// the double-buffer holdover.
    pub fn submit_nv12(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        video_data: Vec<u8>,
        audio: &[AudioFrame],
    ) {
        // 1. Audio first — fast, non-blocking, goes straight into NDI's queue.
        for af in audio {
            self.sender.send_audio(af);
        }

        // 2. Video async — may block on clock_video pacing, returns once NDI
        //    has taken ownership of our pointer.
        let frame = VideoFrame {
            data: video_data,
            width,
            height,
            stride,
            frame_rate_n: self.frame_rate_n,
            frame_rate_d: self.frame_rate_d,
            pixel_format: PixelFormat::Nv12,
        };
        // SAFETY: the previous async frame's buffer is held in `prev_frame`
        // below; it will not be dropped until we install the new frame, which
        // happens AFTER this async call returns. The async call is itself the
        // synchronising event that releases the SDK's pointer to the old
        // buffer, per NDIlib_send_send_video_async_v2's documented contract.
        unsafe {
            self.sender.send_video_async(&frame);
        }

        // Install the new frame — this drops whatever was in prev_frame.
        self.prev_frame = Some(frame.data);
    }

    /// Release any pending async frame. Call this on every playback exit path
    /// before allowing the previous frame's Vec to drop.
    pub fn flush(&mut self) {
        self.sender.send_video_flush();
        self.prev_frame = None;
    }

    /// Send a solid-colour BGRA frame synchronously — used for idle /
    /// paused states. Internally flushes any pending async frame first.
    pub fn send_black_bgra(&mut self, width: u32, height: u32) {
        self.flush();
        let data = vec![0u8; (width * height * 4) as usize];
        let frame = VideoFrame {
            data,
            width,
            height,
            stride: width * 4,
            frame_rate_n: self.frame_rate_n,
            frame_rate_d: self.frame_rate_d,
            pixel_format: PixelFormat::Bgra,
        };
        self.sender.send_video(&frame);
    }

    /// Borrow the underlying sender (mainly for tests).
    pub fn sender(&self) -> &NdiSender<B> {
        &self.sender
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_ndi::test_util::MockNdiBackend;
    use std::sync::Arc;

    fn mk_audio(interleaved: Vec<f32>, channels: u32) -> AudioFrame {
        AudioFrame {
            data: interleaved,
            channels,
            sample_rate: 48000,
        }
    }

    #[test]
    fn submit_sends_audio_before_video_async() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "S", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        let audio = vec![mk_audio(vec![0.1, 0.2, 0.3, 0.4], 2)];
        sub.submit_nv12(4, 2, 4, vec![0u8; 4 * 2 * 3 / 2], &audio);

        let calls = backend.calls();
        // Expect: create (with clocking), send_audio, send_video_async
        assert_eq!(calls[0], "send_create_with_clocking(S,true,false)");
        assert_eq!(calls[1], "send_audio(42,sr=48000,ch=2,spc=2)");
        assert_eq!(calls[2], "send_video_async(42,NV12,4x2,stride=4,30/1)");
    }

    #[test]
    fn submit_handles_multiple_audio_chunks_in_order() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "M", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        let audio = vec![
            mk_audio(vec![1.0, 2.0], 2),
            mk_audio(vec![3.0, 4.0, 5.0, 6.0], 2),
        ];
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &audio);

        let calls = backend.calls();
        // create, audio chunk 1, audio chunk 2, video
        assert!(calls[1].starts_with("send_audio(42,sr=48000,ch=2,spc=1)"));
        assert!(calls[2].starts_with("send_audio(42,sr=48000,ch=2,spc=2)"));
        assert!(calls[3].starts_with("send_video_async"));
    }

    #[test]
    fn flush_is_recorded_and_clears_prev_frame() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "F", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.flush();

        let calls = backend.calls();
        assert!(calls.iter().any(|c| c == "send_video_flush(42)"));
        assert!(sub.prev_frame.is_none());
    }

    #[test]
    fn send_black_bgra_flushes_first() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "K", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.send_black_bgra(1920, 1080);

        let calls = backend.calls();
        // Must see: create, send_video_async (NV12), send_video_flush, send_video (BGRA)
        let idx_async = calls
            .iter()
            .position(|c| c.starts_with("send_video_async"))
            .unwrap();
        let idx_flush = calls
            .iter()
            .position(|c| c == "send_video_flush(42)")
            .unwrap();
        // Assert the exact call string including the stride value — this kills the
        // `stride: width * 4` mutants (+4 would give 1924, /4 would give 480,
        // both would not match the expected 7680).
        let idx_black = calls
            .iter()
            .position(|c| c == "send_video(42,BGRA,1920x1080,stride=7680,30/1)")
            .unwrap();
        assert!(idx_async < idx_flush);
        assert!(idx_flush < idx_black);
    }

    #[test]
    fn drop_flushes_before_destroy_via_sender() {
        let backend = Arc::new(MockNdiBackend::new());
        {
            let sender = NdiSender::new_with_clocking(backend.clone(), "D", true, false).unwrap();
            let mut sub = FrameSubmitter::new(sender, 30, 1);
            sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
            // sub drops here → sender drops → flush + destroy
        }
        let calls = backend.calls();
        // The last two calls must be flush then destroy (flush on drop + destroy).
        let last_two = &calls[calls.len() - 2..];
        assert_eq!(last_two[0], "send_video_flush(42)");
        assert_eq!(last_two[1], "send_destroy(42)");
    }

    #[test]
    fn frame_rate_is_forwarded_to_video_frame() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "R", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 60000, 1001);
        sub.submit_nv12(1920, 1080, 1920, vec![0u8; 1920 * 1080 * 3 / 2], &[]);
        let calls = backend.calls();
        assert!(
            calls.iter().any(|c| c.contains("60000/1001")),
            "expected 60000/1001 in one of the calls: {calls:#?}"
        );
    }

    #[test]
    fn set_frame_rate_updates_subsequent_frames() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "U", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.set_frame_rate(60, 1);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        let calls = backend.calls();
        let async_calls: Vec<_> = calls
            .iter()
            .filter(|c| c.starts_with("send_video_async"))
            .collect();
        assert_eq!(async_calls.len(), 2);
        assert!(async_calls[0].contains("30/1"));
        assert!(async_calls[1].contains("60/1"));
    }
}
