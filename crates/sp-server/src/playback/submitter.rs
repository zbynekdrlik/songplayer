//! Frame submission helper for the `SP-program` NDI output.
//!
//! #221 lane 3: `SP-program` is SongPlayer's only NDI sender (a playlist feeds
//! the program bus and has no NDI output of its own), so this submitter is
//! the program output's (`program_output.rs`). It owns the `NdiSender` and
//! enforces the rules required for correct NDI output:
//!
//! 1. For each boundary, audio chunks are submitted BEFORE the video frame.
//!
//! 2. The previous video frame's buffer is kept alive until the next
//!    `submit` or `flush` call. `NDIlib_send_send_video_async_v2` retains a
//!    pointer to our bytes and only releases it when the next async/sync/flush
//!    call arrives.
//!
//! 3. `flush` is called on the program output's exit path. Flush itself is a
//!    sync point that releases the previous frame, after which the buffer may
//!    be dropped.

use sp_ndi::{AudioFrame, NdiBackend, NdiSender, PixelFormat};

use crate::playback::fleet_shift::wire_stamp_100ns as wire;
use crate::playback::frame_buf::{BlackNv12, SharedFrame};
use crate::playback::wallclock::WallClock;

/// Owns an `NdiSender` plus the previous frame's buffer for the async
/// double-buffer pattern.
///
/// **SAFETY-CRITICAL:** the field declaration order is load-bearing.
/// `sender` MUST be declared before `prev_frame` so that on `Drop` Rust
/// destroys the sender first (which calls `NdiSender::Drop` →
/// `send_video_flush` → releases NDI's retained pointer to
/// `prev_frame`'s bytes), THEN drops `prev_frame`, releasing the shared
/// buffer's last `Arc` reference only after NDI has confirmed it no longer
/// needs it. Reversing the order would drop the pixels while NDI still held a
/// pointer to them — silent use-after-free in the SDK. See `NdiSender::Drop`
/// in `crates/sp-ndi/src/sender.rs`.
pub struct FrameSubmitter<B: NdiBackend> {
    // NOTE: do not reorder these fields — see the SAFETY-CRITICAL note above.
    sender: NdiSender<B>,
    /// Keeps the previous async frame's pixels alive until NDI releases its
    /// pointer (which happens when the next submit / flush call fires). A
    /// [`SharedFrame`] (`Arc<Vec<u8>>`) so the holdover is a refcount hold with
    /// ZERO pixel copy (#203) — the buffer the SDK still points at survives
    /// exactly until the next submit installs a new one.
    prev_frame: Option<SharedFrame>,
    frame_rate_n: i32,
    frame_rate_d: i32,
    /// Monotonic count of submitted pairs.
    frames_submitted_total: u64,
    /// The wall clock whose fleet relabel registry puts the wire labels on
    /// every pair (#224 part 2). Never ticked here: the pairs arrive stamped.
    wall: WallClock,
    /// #147 standby same-path: the program's NV12 standby black, built ONCE
    /// and handed out by `Arc` clone ([`standby_black_nv12`](Self::standby_black_nv12)).
    /// Declared after `sender`, so the SDK's pointer is released before this
    /// last `Arc` drops.
    black_nv12: BlackNv12,
}

impl<B: NdiBackend> FrameSubmitter<B> {
    /// Create a submitter owning an already-constructed sender. Delegates to
    /// [`new_with_wallclock`](Self::new_with_wallclock) with the production
    /// system clock — no throwaway clock is constructed.
    pub fn new(sender: NdiSender<B>, frame_rate_n: i32, frame_rate_d: i32) -> Self {
        Self::new_with_wallclock(sender, frame_rate_n, frame_rate_d, WallClock::system())
    }

    /// Construct with an injected [`WallClock`] (its relabel registry) so the
    /// wire stamps are deterministic in tests; [`new`](Self::new) delegates
    /// here with the system clock. The injected clock is stored and used
    /// directly.
    pub fn new_with_wallclock(
        sender: NdiSender<B>,
        frame_rate_n: i32,
        frame_rate_d: i32,
        wall: WallClock,
    ) -> Self {
        Self {
            sender,
            prev_frame: None,
            frame_rate_n,
            frame_rate_d,
            frames_submitted_total: 0,
            wall,
            black_nv12: BlackNv12::default(),
        }
    }

    /// Release any pending async frame. Call this on the exit path before
    /// allowing the previous frame's buffer to drop.
    pub fn flush(&mut self) {
        self.sender.send_video_flush();
        self.prev_frame = None;
    }

    /// Submit one boundary-paced frame at EXPLICIT genlock timecodes (#147).
    ///
    /// Audio chunks first (stamped `audio_tc_100ns`, the timeline instant of
    /// the block — every paced caller passes its boundary, #224), then the
    /// video frame async (stamped `video_tc_100ns`, the floored
    /// boundary — §4). Both stamps are on the internal timeline and go on the
    /// fleet labels here (#224 part 2: `floor_boundary(b + D(K_F))`, K_F read
    /// from the wall's relabel registry once per pair).
    /// The borrowed video is copied for the async
    /// double-buffer holdover, into a RECYCLED pool buffer
    /// ([`SharedFrame::copy_from_slice`], #147 round 10) — never a fresh alloc.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_frame_at_boundary(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        video_data: &[u8],
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        // Borrow variant: copy the caller's buffer into a pooled SharedFrame and
        // delegate (for a caller that keeps its own slice). The pacer's
        // `PacedSink` impl and the program output both use the zero-copy
        // `_owned` variant instead.
        self.submit_frame_at_boundary_owned(
            width,
            height,
            stride,
            SharedFrame::copy_from_slice(video_data),
            audio,
            video_tc_100ns,
            audio_tc_100ns,
        );
    }

    /// Same as [`submit_frame_at_boundary`](Self::submit_frame_at_boundary) but
    /// takes a [`SharedFrame`] whose `Arc` is MOVED straight into the async
    /// double-buffer holdover with NO pixel copy (#168 + #203). The send is by
    /// borrowed slice and the same handle becomes `prev_frame`, so the SDK's
    /// pointer stays valid with zero copies.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_frame_at_boundary_owned(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        video: SharedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        self.frames_submitted_total += 1;
        // #224 part 2: the ONE edge where the fleet labels go on. The video
        // stamp is an internal boundary b: on the wire it is b's boundary
        // K_F slots later, floored — never future-dated. K_F is read once and
        // the audio stamp moves by the SAME relabel, so a source's own audio
        // offset survives (the program forwards a source's stamps).
        let video_wire = wire(video_tc_100ns, self.wall.fleet().slots());
        let audio_tc_100ns = audio_tc_100ns + (video_wire - video_tc_100ns);
        let video_tc_100ns = video_wire;

        // 1. Audio first — the boundary's chunks go into NDI's queue before
        //    the video frame (the audio-first invariant, module doc).
        for af in audio {
            let mut stamped = af.clone();
            stamped.timecode_100ns = Some(audio_tc_100ns);
            self.sender.send_audio(&stamped);
        }

        // 2. Video async, stamped with the floored boundary.
        // SAFETY: `prev_frame` holds the previous async buffer's Arc until this
        // async call releases the SDK's pointer to it; the new SharedFrame is
        // installed immediately after — the holdover is a refcount hold, zero copy.
        unsafe {
            self.sender.send_video_async_slice(
                width,
                height,
                stride,
                self.frame_rate_n,
                self.frame_rate_d,
                PixelFormat::Nv12,
                Some(video_tc_100ns),
                &video[..],
            );
        }
        self.prev_frame = Some(video);
    }

    /// Borrow the underlying sender (the program output polls its receivers).
    pub fn sender(&self) -> &NdiSender<B> {
        &self.sender
    }

    pub fn frames_submitted_total(&self) -> u64 {
        self.frames_submitted_total
    }

    /// The standby NV12 black for `width`×`height`, cached for the output's
    /// life and handed out by `Arc` clone (a refcount bump, no pixel copy). A
    /// different size rebuilds it once.
    pub fn standby_black_nv12(&mut self, width: u32, height: u32) -> SharedFrame {
        self.black_nv12.get(width, height)
    }

    /// The picture the async holdover keeps alive for the SDK: the last one
    /// sent. A test seam (#223): the tests read the wire picture's bytes.
    #[cfg(test)]
    pub(crate) fn held_frame(&self) -> Option<&SharedFrame> {
        self.prev_frame.as_ref()
    }
}

/// A `FrameSubmitter` is a [`PacedSink`](crate::playback::pacer::PacedSink):
/// a pacer can emit straight into an NDI sender (the pacer tests drive it so,
/// over `MockNdiBackend`).
impl<B: NdiBackend> crate::playback::pacer::PacedSink for FrameSubmitter<B> {
    fn emit(
        &mut self,
        video: &crate::playback::pacer::PacedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        // `audio` is the boundary's media-aligned block (#148), NOT
        // `video.audio` — the pacer moves that into its grid buffer on pull.
        // #147 round 10: an Arc bump of the pacer's frame, never a pixel copy.
        self.submit_frame_at_boundary_owned(
            video.width,
            video.height,
            video.stride,
            video.video.clone(),
            audio,
            video_tc_100ns,
            audio_tc_100ns,
        );
    }

    /// Zero-copy standby submit (#203): move the shared handle straight into the
    /// async holdover — a refcount hold, no `to_vec`. Overrides the trait default
    /// (which copies via `emit`) so a standby submits the SAME allocation every
    /// boundary.
    fn submit_shared(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        video: SharedFrame,
        audio: &[AudioFrame],
        video_tc_100ns: i64,
        audio_tc_100ns: i64,
    ) {
        self.submit_frame_at_boundary_owned(
            width,
            height,
            stride,
            video,
            audio,
            video_tc_100ns,
            audio_tc_100ns,
        );
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
            timecode_100ns: None,
        }
    }

    #[test]
    fn submit_sends_audio_before_video_async() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "S", false, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        let audio = vec![mk_audio(vec![0.1, 0.2, 0.3, 0.4], 2)];
        sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &audio, 333_333, 333_333);

        let calls = backend.calls();
        // Expect: create (with clocking), send_audio, send_video_async
        assert_eq!(calls[0], "send_create_with_clocking(S,false,false)");
        assert_eq!(calls[1], "send_audio(42,sr=48000,ch=2,spc=2)");
        assert_eq!(calls[2], "send_video_async(42,NV12,4x2,stride=4,30/1)");
    }

    #[test]
    fn submit_handles_multiple_audio_chunks_in_order() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "M", false, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        let audio = vec![
            mk_audio(vec![1.0, 2.0], 2),
            mk_audio(vec![3.0, 4.0, 5.0, 6.0], 2),
        ];
        sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &audio, 333_333, 333_333);

        let calls = backend.calls();
        // create, audio chunk 1, audio chunk 2, video
        assert!(calls[1].starts_with("send_audio(42,sr=48000,ch=2,spc=1)"));
        assert!(calls[2].starts_with("send_audio(42,sr=48000,ch=2,spc=2)"));
        assert!(calls[3].starts_with("send_video_async"));
    }

    #[test]
    fn flush_is_recorded_and_clears_prev_frame() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "F", false, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &[], 333_333, 333_333);
        sub.flush();

        let calls = backend.calls();
        assert!(calls.iter().any(|c| c == "send_video_flush(42)"));
        assert!(sub.prev_frame.is_none());
    }

    #[test]
    fn drop_flushes_before_destroy_via_sender() {
        let backend = Arc::new(MockNdiBackend::new());
        {
            let sender = NdiSender::new_with_clocking(backend.clone(), "D", false, false).unwrap();
            let mut sub = FrameSubmitter::new(sender, 30, 1);
            sub.submit_frame_at_boundary(4, 2, 4, &[0u8; 12], &[], 333_333, 333_333);
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
        let sender = NdiSender::new_with_clocking(backend.clone(), "R", false, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 60000, 1001);
        let data = vec![0u8; 1920 * 1080 * 3 / 2];
        sub.submit_frame_at_boundary(1920, 1080, 1920, &data, &[], 333_333, 333_333);
        let calls = backend.calls();
        assert!(
            calls.iter().any(|c| c.contains("60000/1001")),
            "expected 60000/1001 in one of the calls: {calls:#?}"
        );
    }

    #[test]
    fn multi_frame_submission_records_five_async_calls_in_order() {
        // Exercises the prev_frame holdover across multiple submits: each
        // send_video_async is a synchronising event that releases the
        // previous frame's pointer, so the submitter should be willing to
        // accept N sequential submits without leaking buffers or crashing.
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Seq", false, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        for i in 0..5i64 {
            // Distinct buffers each time so any UAF would corrupt the assertion.
            let data = vec![i as u8; 4 * 2 * 3 / 2];
            let b = (i + 1) * 333_333;
            sub.submit_frame_at_boundary(4, 2, 4, &data, &[], b, b);
        }

        let async_calls: Vec<_> = backend
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("send_video_async"))
            .collect();
        assert_eq!(
            async_calls.len(),
            5,
            "expected 5 async sends, got {}: {async_calls:#?}",
            async_calls.len()
        );
        // prev_frame must be Some (the last frame's buffer, held for the next sync event).
        assert!(sub.prev_frame.is_some());
        sub.flush();
        assert!(sub.prev_frame.is_none());
    }
}

// #147 standby same-path: the cached NV12 black (tests).
#[cfg(test)]
#[path = "submitter_tests_standby.rs"]
mod submitter_tests_standby;

#[cfg(test)]
#[path = "submitter_tests_timecode.rs"]
mod submitter_tests_timecode;

#[cfg(test)]
#[path = "submitter_tests_mutants.rs"]
mod submitter_tests_mutants;

#[cfg(test)]
#[path = "submitter_tests_regrid.rs"]
mod submitter_tests_regrid;
