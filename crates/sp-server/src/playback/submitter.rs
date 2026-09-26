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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns};
use sp_ndi::{AudioFrame, NdiBackend, NdiSender, PixelFormat, VideoFrame};

use crate::playback::frame_buf::SharedFrame;
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
    /// Monotonic count of `submit_nv12` calls. `send_black_bgra` does not
    /// bump this — black-frame standby is not "playback" for visibility
    /// purposes.
    frames_submitted_total: u64,
    /// Frames since the most recent `drain_window` call. Reset on drain.
    frames_in_window: u32,
    /// Wall-clock instant the current window started (last drain or
    /// FrameSubmitter construction).
    window_start: std::time::Instant,
    /// Wall-clock instant of the last `submit_nv12` call. `None` means no
    /// real frame has been submitted (standby black frames are excluded).
    last_submit_ts: Option<std::time::Instant>,
    /// Monotonic-to-UTC wall clock stamping genlock timecodes onto every real
    /// frame (#146). One clock per pipeline thread, owned here.
    wall: WallClock,
    /// `genlock_pacing` for this pipeline (#147). ON = the app owns the cadence,
    /// so a standby/black frame is stamped with its on-grid boundary rather than
    /// `SYNTHESIZE` (contract §4.3 — a real send is never SYNTHESIZE; the legacy
    /// SDK-clocked path keeps `None`). Default OFF.
    paced: bool,
    /// Runtime burn-id QR overlay toggle (#151), shared with the API via
    /// `NdiBurnRegistry`. Read fresh on every paced boundary emit
    /// ([`submit_frame_at_boundary`](Self::submit_frame_at_boundary)) so a toggle
    /// takes effect within one frame. Default OFF; never persisted; only the
    /// paced path paints (the legacy `submit_nv12` path never reads it).
    burn_on: Arc<AtomicBool>,
    /// #192 round 3: per-call `send_video_async` durations (µs), so the
    /// heartbeat can tell a producer stall caused by the SDK video submit apart
    /// from a decode / audio stall. Drained (max, p99) each `drain_window`.
    submit_times: crate::playback::loop_stats::SubmitHist,
    /// #203: the standby BGRA black buffer, REUSED across `send_black_bgra` calls
    /// keyed by size. `send_video` is synchronous, so the buffer is free the
    /// moment the call returns and can be handed to the next standby frame with
    /// no re-alloc — removing the paused-branch 8.3 MB `VirtualAlloc`/`VirtualFree`
    /// churn (~83 MB/s per paused output). Reallocated only on a `(width,height)`
    /// change.
    black_bgra: Option<Vec<u8>>,
    /// #147 standby same-path: the paced idle NV12 black `(width, height,
    /// frame)`, built ONCE per pipeline and handed out by `Arc` clone
    /// ([`standby_black_nv12`](Self::standby_black_nv12)). Declared after
    /// `sender`, so the SDK's pointer is released before this last `Arc` drops.
    black_nv12: Option<(u32, u32, SharedFrame)>,
}

impl<B: NdiBackend> FrameSubmitter<B> {
    /// Create a submitter owning an already-constructed sender. Delegates to
    /// [`new_with_wallclock`](Self::new_with_wallclock) with the production
    /// system clock — no throwaway clock is constructed.
    pub fn new(sender: NdiSender<B>, frame_rate_n: i32, frame_rate_d: i32) -> Self {
        Self::new_with_wallclock(sender, frame_rate_n, frame_rate_d, WallClock::system())
    }

    /// Construct with an injected [`WallClock`] so genlock timecodes are
    /// deterministic in tests; [`new`](Self::new) delegates here with the
    /// system clock. The injected clock is stored and used directly.
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
            frames_in_window: 0,
            window_start: std::time::Instant::now(),
            last_submit_ts: None,
            wall,
            paced: false,
            burn_on: Arc::new(AtomicBool::new(false)),
            submit_times: crate::playback::loop_stats::SubmitHist::default(),
            black_bgra: None,
            black_nv12: None,
        }
    }

    /// Install the shared burn-id overlay flag (#151) for this pipeline. Called
    /// once at paced-pipeline start with the `Arc<AtomicBool>` the
    /// `NdiBurnRegistry` also holds, so the runtime API toggle and this
    /// submitter read the same atomic. The default flag from
    /// [`new`](Self::new) is OFF, so a submitter that never gets one never burns.
    pub fn set_burn_flag(&mut self, flag: Arc<AtomicBool>) {
        self.burn_on = flag;
    }

    /// Whether the burn-id overlay is currently ON for this pipeline (#151).
    pub fn burn_active(&self) -> bool {
        self.burn_on.load(Ordering::Relaxed)
    }

    /// Set the `genlock_pacing` flag (#147). Called once at pipeline-thread
    /// start with `genlock_pacing`; when ON, standby/black frames are stamped
    /// with their on-grid boundary instead of `SYNTHESIZE`.
    pub fn set_paced(&mut self, paced: bool) {
        self.paced = paced;
    }

    /// Update the frame rate used for subsequent submissions. Call this when
    /// a new file is opened and its real frame rate is known.
    ///
    /// Defensive: if either value is non-positive (malformed MF media type),
    /// falls back to 30000/1001 with a warning rather than handing NDI a
    /// division-by-zero frame rate.
    pub fn set_frame_rate(&mut self, num: i32, den: i32) {
        if num > 0 && den > 0 {
            self.frame_rate_n = num;
            self.frame_rate_d = den;
        } else {
            tracing::warn!(
                num,
                den,
                "invalid frame rate received, falling back to 30000/1001"
            );
            self.frame_rate_n = 30_000;
            self.frame_rate_d = 1_001;
        }
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
        // Counters first — these must run on every successful submit, even
        // if the SDK call below blocks on clock_video pacing.
        self.frames_submitted_total += 1;
        self.frames_in_window += 1;
        self.last_submit_ts = Some(std::time::Instant::now());

        // Genlock (#146): advance the wall clock once per submit, take ONE
        // wall reading, and stamp both streams from it. Audio carries the raw
        // wall clock (no snap, §6); video carries the floored grid boundary
        // (§4, FLOOR never ceil).
        self.wall.tick();
        let now_100ns = self.wall.now_100ns();
        let audio_tc = Some(now_100ns);
        let video_tc = Some(floor_boundary_100ns(now_100ns, GENLOCK_GRID_FPS));

        // 1. Audio first — fast, non-blocking, goes straight into NDI's queue.
        for af in audio {
            let mut stamped = af.clone();
            stamped.timecode_100ns = audio_tc;
            self.sender.send_audio(&stamped);
        }

        // 2. Video async — may block on clock_video pacing, returns once NDI
        //    has taken ownership of our pointer. Pacing stays SDK-clocked in
        //    #146; #147 replaces it with boundary-paced emission. Wrap the owned
        //    pixels ONCE in a `SharedFrame` (a small Arc header, NO pixel copy)
        //    and send by borrowed slice so the holdover is a refcount hold
        //    (#203).
        let video = SharedFrame::new(video_data);
        // SAFETY: the previous async frame's buffer is held in `prev_frame`
        // below; it will not be dropped until we install the new frame, which
        // happens AFTER this async call returns. The async call is itself the
        // synchronising event that releases the SDK's pointer to the old
        // buffer, per NDIlib_send_send_video_async_v2's documented contract.
        // #192 round 3: time the SDK call — it blocks on the prior async frame,
        // so under a resident heavy child it is the candidate stalling stage.
        let (_, submit_us) = crate::playback::loop_stats::timed(|| unsafe {
            self.sender.send_video_async_slice(
                width,
                height,
                stride,
                self.frame_rate_n,
                self.frame_rate_d,
                PixelFormat::Nv12,
                video_tc,
                &video[..],
            );
        });
        self.submit_times.observe(submit_us);

        // Install the new frame — this drops whatever was in prev_frame.
        self.prev_frame = Some(video);
    }

    /// Release any pending async frame. Call this on every playback exit path
    /// before allowing the previous frame's Vec to drop.
    pub fn flush(&mut self) {
        self.sender.send_video_flush();
        self.prev_frame = None;
    }

    /// Send a solid-colour BGRA frame synchronously — used for idle /
    /// paused states. Internally flushes any pending async frame first.
    ///
    /// #203: reuses the [`black_bgra`](Self::black_bgra) buffer across calls of
    /// the same size (black BGRA is all zeros, so a reused buffer is already
    /// zeroed and a fresh one is zero-initialised); `send_video` is synchronous,
    /// so the buffer returns to us the instant the call ends.
    pub fn send_black_bgra(&mut self, width: u32, height: u32) {
        self.flush();
        let needed = (width * height * 4) as usize;
        // Reuse the cached standby buffer when the size matches (black BGRA is all
        // zeros, so a reused buffer is already zeroed); reallocate only on a size
        // change.
        let data = match self.black_bgra.take() {
            Some(buf) if buf.len() == needed => buf,
            _ => vec![0u8; needed],
        };
        // Paced (#147): a real send is never SYNTHESIZE — stamp the standby
        // frame with the floored on-grid boundary at the send instant (§4.3), so
        // an idle→play transition does not drop the receiver out of `locked=`.
        // The legacy SDK-clocked path keeps `None` (SYNTHESIZE, open question 7).
        let timecode_100ns = if self.paced {
            Some(floor_boundary_100ns(
                self.wall.now_100ns(),
                GENLOCK_GRID_FPS,
            ))
        } else {
            None
        };
        let frame = VideoFrame {
            data,
            width,
            height,
            stride: width * 4,
            frame_rate_n: self.frame_rate_n,
            frame_rate_d: self.frame_rate_d,
            pixel_format: PixelFormat::Bgra,
            timecode_100ns,
        };
        self.sender.send_video(&frame);
        // Reclaim the buffer for the next standby frame (send_video is sync).
        self.black_bgra = Some(frame.data);
    }

    /// The current standby BGRA buffer's start pointer (`as usize`), or `None`
    /// before the first `send_black_bgra`. Test-only: proves the buffer is REUSED
    /// (same pointer) across same-size calls and reallocated on a size change.
    #[cfg(test)]
    fn black_bgra_ptr(&self) -> Option<usize> {
        self.black_bgra.as_ref().map(|b| b.as_ptr() as usize)
    }

    /// Submit one boundary-paced frame at EXPLICIT genlock timecodes (#147).
    ///
    /// Audio chunks first (stamped `audio_tc_100ns`, the raw wall clock — §6),
    /// then the video frame async (stamped `video_tc_100ns`, the floored
    /// boundary — §4). Unlike [`submit_nv12`](Self::submit_nv12) the `Pacer`
    /// owns the wall clock and supplies both stamps, so this bypasses the
    /// internal [`WallClock`]. The borrowed video is copied for the async
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
        // `PacedSink` impl and the #168 submit thread both use the zero-copy
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
    /// double-buffer holdover with NO pixel copy (#168 + #203). The #168 submit
    /// thread already owns the job's pixels, so wrapping them in a `SharedFrame`
    /// is one small Arc header; the send is by borrowed slice and the same handle
    /// becomes `prev_frame`, so the SDK's pointer stays valid with zero copies on
    /// the SDK-blocking submit thread.
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
        self.frames_in_window += 1;
        self.last_submit_ts = Some(std::time::Instant::now());

        // 1. Audio first — the boundary's chunks go into NDI's queue before
        //    the video frame (the audio-first invariant, submitter.rs top).
        for af in audio {
            let mut stamped = af.clone();
            stamped.timecode_100ns = Some(audio_tc_100ns);
            self.sender.send_audio(&stamped);
        }

        // 2. Video async, stamped with the floored boundary.
        //
        // #151 burn-id overlay: paint the QR into OUR OWN copy of the frame
        // (`SharedFrame::make_mut` — the pacer keeps its own clone for the
        // starvation repeat, so this forks into a POOLED copy, #147 round 10),
        // NEVER the decoder's / pacer's buffer, and re-derives a fresh payload
        // every boundary. Paced path
        // only; read the shared flag fresh so a toggle-off clears within one
        // frame. `frame_id` = the pacing `seq` (== `frames_submitted_total`,
        // bumped above); `gen_ts_ns` = the serviced boundary wall time in ns
        // (`video_tc_100ns` is in 100-ns units).
        let mut video = video;
        if self.burn_on.load(Ordering::Relaxed) {
            crate::playback::burn_overlay::paint_burn(
                video.make_mut(),
                width,
                height,
                stride,
                self.frames_submitted_total as u32,
                video_tc_100ns.saturating_mul(100),
            );
        }
        // SAFETY: `prev_frame` holds the previous async buffer's Arc until this
        // async call releases the SDK's pointer to it; the new SharedFrame is
        // installed immediately after — the holdover is a refcount hold, zero copy.
        // #192 round 3: time the SDK call (same gauge as the SDK-clocked path).
        let (_, submit_us) = crate::playback::loop_stats::timed(|| unsafe {
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
        });
        self.submit_times.observe(submit_us);
        self.prev_frame = Some(video);
    }

    /// Submit an audio-only tail chunk at an explicit timecode (#148 rework,
    /// item 4). Used at EOS to flush the last partial boundary of buffered audio
    /// (zero-filled to `samples_per_boundary`) — there is no accompanying video
    /// frame, so this does NOT touch the video double-buffer or the frame
    /// counters; it only stamps and sends the audio chunk(s).
    pub fn submit_audio_tail(&mut self, audio: &[AudioFrame], audio_tc_100ns: i64) {
        for af in audio {
            let mut stamped = af.clone();
            stamped.timecode_100ns = Some(audio_tc_100ns);
            self.sender.send_audio(&stamped);
        }
    }

    /// Borrow the underlying sender (mainly for tests).
    pub fn sender(&self) -> &NdiSender<B> {
        &self.sender
    }

    /// A cloneable audio-only send handle over this submitter's sender (#192),
    /// for the wall-clock audio emitter thread. NDI permits audio and video to
    /// be submitted from separate threads on the same sender; see
    /// [`sp_ndi::NdiSender::audio_sink`] for the teardown-ordering contract.
    pub fn audio_sink(&self) -> sp_ndi::AudioSink<B> {
        self.sender.audio_sink()
    }

    /// Snapshot the rolling window counter and reset it. Returns the number
    /// of frames submitted since the last drain plus the wall-clock seconds
    /// over which they accumulated.
    ///
    /// The heartbeat caller divides `frames_in_window / window_secs` to get
    /// observed fps. `window_secs` is clamped at the call site to avoid
    /// divide-by-zero on freshly-spawned pipelines (the heartbeat does
    /// `window_secs.max(0.001)`).
    pub fn drain_window(&mut self) -> crate::playback::ndi_health::WindowStats {
        let now = std::time::Instant::now();
        let window_secs = now.duration_since(self.window_start).as_secs_f32();
        let frames = self.frames_in_window;
        self.frames_in_window = 0;
        self.window_start = now;
        // #192 round 3: drain the per-call send_video_async gauge for the window.
        let (submit_call_us_max, submit_call_us_p99) = self.submit_times.drain();
        crate::playback::ndi_health::WindowStats {
            frames_in_window: frames,
            window_secs,
            drained_at: now,
            submit_call_us_max,
            submit_call_us_p99,
        }
    }

    /// Drain the per-call `send_video_async` gauge — the SAME `SubmitHist`
    /// `drain_window` reads on the SDK-clocked path, but WITHOUT touching the
    /// frame-count window (the #168 submit thread owns this submitter and the
    /// paced heartbeat counts frames via `SubmitCounters`, not `drain_window`).
    /// Returns `(max, p99)` µs for the drained window and clears it.
    pub fn drain_submit_call_us(&mut self) -> (u64, u64) {
        self.submit_times.drain()
    }

    /// Test seam: feed a known call duration into the gauge (the real samples
    /// come from `Instant` timing around `send_video_async`, which a unit test
    /// cannot pin to an exact value).
    #[cfg(test)]
    pub(crate) fn observe_submit_call_us(&mut self, us: u64) {
        self.submit_times.observe(us);
    }

    pub fn frames_submitted_total(&self) -> u64 {
        self.frames_submitted_total
    }

    pub fn last_submit_ts(&self) -> Option<std::time::Instant> {
        self.last_submit_ts
    }

    pub fn frame_rate_n(&self) -> i32 {
        self.frame_rate_n
    }

    pub fn frame_rate_d(&self) -> i32 {
        self.frame_rate_d
    }

    /// Current nominal frame rate as fps. Used by the heartbeat to compute
    /// the underrun threshold (observed_fps < nominal_fps / 2).
    pub fn nominal_fps(&self) -> f32 {
        if self.frame_rate_d == 0 {
            return 0.0;
        }
        self.frame_rate_n as f32 / self.frame_rate_d as f32
    }
}

/// The `FrameSubmitter` is the production [`PacedSink`](crate::playback::pacer::PacedSink)
/// for the boundary-paced emission loop (#147).
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
        // #147 round 10: an Arc bump of the pacer's frame, never a pixel copy
        // (the same zero-copy holdover the #168 handoff uses; the burn overlay
        // forks its own copy via `make_mut`, so the pacer's pixels stay intact).
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
    /// (which copies via `emit`) so the idle Black loop submits the SAME
    /// allocation every boundary.
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
    fn drain_submit_call_us_returns_exact_max_and_p99_then_clears() {
        // #168 r2: the paced heartbeat reads this gauge; exact values kill the
        // `(0, 1)` / `(1, 0)` / `(1, 1)` replacement mutants.
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend, "S", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);
        sub.observe_submit_call_us(500);
        sub.observe_submit_call_us(900);
        assert_eq!(sub.drain_submit_call_us(), (900, 900));
        // Drained: an empty window reads (0, 0).
        assert_eq!(sub.drain_submit_call_us(), (0, 0));
        sub.observe_submit_call_us(120);
        assert_eq!(sub.drain_submit_call_us(), (120, 120));
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

    #[test]
    fn set_frame_rate_rejects_zero_denominator() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Z", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);
        sub.set_frame_rate(60, 0); // malformed — should fall back
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        let calls = backend.calls();
        // The submitted frame must carry the fallback rate, not 60/0.
        assert!(
            calls.iter().any(|c| c.contains("30000/1001")),
            "expected fallback 30000/1001 in calls: {calls:#?}"
        );
    }

    #[test]
    fn set_frame_rate_rejects_zero_numerator() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Z2", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);
        sub.set_frame_rate(0, 1);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        let calls = backend.calls();
        assert!(
            calls.iter().any(|c| c.contains("30000/1001")),
            "expected fallback 30000/1001 in calls: {calls:#?}"
        );
    }

    #[test]
    fn multi_frame_submission_records_five_async_calls_in_order() {
        // Exercises the prev_frame holdover across multiple submits: each
        // send_video_async is a synchronising event that releases the
        // previous frame's pointer, so the submitter should be willing to
        // accept N sequential submits without leaking buffers or crashing.
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Seq", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        for i in 0..5 {
            // Distinct Vec each time so any UAF would corrupt the assertion.
            let data = vec![i as u8; 4 * 2 * 3 / 2];
            sub.submit_nv12(4, 2, 4, data, &[]);
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

    #[test]
    fn submitter_counts_frames_submitted_total() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Cnt", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        assert_eq!(sub.frames_submitted_total(), 0);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        assert_eq!(sub.frames_submitted_total(), 1);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        assert_eq!(sub.frames_submitted_total(), 3);
    }

    #[test]
    fn drain_window_resets_window_counter_but_not_total() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "DW", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        let stats1 = sub.drain_window();
        assert_eq!(stats1.frames_in_window, 2);
        assert!(stats1.window_secs >= 0.0);
        // Total preserved across drain.
        assert_eq!(sub.frames_submitted_total(), 2);

        // Next drain (no submits in between) returns 0 frames.
        let stats2 = sub.drain_window();
        assert_eq!(stats2.frames_in_window, 0);
        assert_eq!(sub.frames_submitted_total(), 2);

        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        let stats3 = sub.drain_window();
        assert_eq!(stats3.frames_in_window, 1);
        assert_eq!(sub.frames_submitted_total(), 3);
    }

    #[test]
    fn nominal_fps_computes_from_rate_pair() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "F1", true, false).unwrap();
        let sub: FrameSubmitter<_> = FrameSubmitter::new(sender, 30000, 1001);
        let v = sub.nominal_fps();
        assert!((v - 29.97).abs() < 0.01, "expected ~29.97 got {v}");
    }

    #[test]
    fn send_black_bgra_does_not_count_as_a_frame_submission() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "Bk", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.send_black_bgra(1920, 1080);
        assert_eq!(
            sub.frames_submitted_total(),
            0,
            "black-frame standby must NOT count as playback"
        );
        assert!(
            sub.last_submit_ts().is_none(),
            "black-frame standby must not advance last_submit_ts"
        );

        // Confirm a real submit DOES count.
        sub.submit_nv12(4, 2, 4, vec![0u8; 12], &[]);
        assert_eq!(sub.frames_submitted_total(), 1);
        assert!(sub.last_submit_ts().is_some());
    }

    #[test]
    fn send_black_bgra_reuses_its_buffer_across_same_size_calls() {
        // #203: the standby BGRA buffer is reused across same-size calls (the send
        // is synchronous, so the buffer is free the instant it returns) and
        // reallocated only when the size changes.
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "BB", false, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);

        sub.send_black_bgra(320, 240);
        // The standby frame is EXACTLY width * height * 4 BGRA bytes — a wrong
        // size arithmetic (w + h, w * h + 4, w * h / 4) would send a torn frame.
        assert_eq!(backend.last_sync_video_len(), Some(320 * 240 * 4));
        let ptr1 = sub
            .black_bgra_ptr()
            .expect("buffer retained after the first standby frame");
        sub.send_black_bgra(320, 240);
        let ptr2 = sub.black_bgra_ptr().unwrap();
        assert_eq!(
            ptr1, ptr2,
            "a same-size standby frame reuses the SAME allocation, no re-alloc"
        );

        // A size change reallocates (the buffer must match the new dimensions).
        sub.send_black_bgra(640, 480);
        assert_eq!(backend.last_sync_video_len(), Some(640 * 480 * 4));
        let ptr3 = sub.black_bgra_ptr().unwrap();
        assert_ne!(ptr2, ptr3, "a size change reallocates the standby buffer");
    }

    #[test]
    fn frame_rate_n_returns_construction_value() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend, "Rn", true, false).unwrap();
        let sub: FrameSubmitter<_> = FrameSubmitter::new(sender, 30000, 1001);
        assert_eq!(sub.frame_rate_n(), 30000);
    }

    #[test]
    fn frame_rate_d_returns_construction_value() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend, "Rd", true, false).unwrap();
        let sub: FrameSubmitter<_> = FrameSubmitter::new(sender, 30000, 1001);
        assert_eq!(sub.frame_rate_d(), 1001);
    }

    // ---- #151 burn-id overlay wiring ----

    #[test]
    fn burn_is_off_by_default() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend, "B0", true, false).unwrap();
        let sub: FrameSubmitter<_> = FrameSubmitter::new(sender, 30, 1);
        assert!(
            !sub.burn_active(),
            "#151: burn overlay defaults OFF (never persisted)"
        );
    }

    #[test]
    fn set_burn_flag_shares_the_atomic_toggle() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend, "B1", true, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);
        let flag = Arc::new(AtomicBool::new(false));
        sub.set_burn_flag(flag.clone());
        assert!(!sub.burn_active());
        flag.store(true, Ordering::Relaxed);
        assert!(
            sub.burn_active(),
            "the API's shared flag drives burn_active within one frame"
        );
        flag.store(false, Ordering::Relaxed);
        assert!(!sub.burn_active());
    }

    #[test]
    fn paced_submit_with_burn_on_still_emits_one_video_frame() {
        let backend = Arc::new(MockNdiBackend::new());
        let sender = NdiSender::new_with_clocking(backend.clone(), "B2", false, false).unwrap();
        let mut sub = FrameSubmitter::new(sender, 30, 1);
        sub.set_burn_flag(Arc::new(AtomicBool::new(true)));
        // 1080p NV12 so the burn geometry fits; the paint runs on our owned copy.
        let (w, h, stride) = (1920u32, 1080u32, 1920u32);
        let data = vec![0u8; (stride * h * 3 / 2) as usize];
        sub.submit_frame_at_boundary(w, h, stride, &data, &[], 3_333_300, 3_333_300);
        let async_calls: Vec<_> = backend
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("send_video_async"))
            .collect();
        assert_eq!(
            async_calls.len(),
            1,
            "a burn-on paced submit must still emit exactly one video frame"
        );
        assert_eq!(sub.frames_submitted_total(), 1);
    }

    /// #151 structural guard: the burn overlay is paced-path ONLY. The file that
    /// hosts the legacy SDK-clocked `decode_and_send` loop (`pipeline.rs`) must
    /// never reference the overlay — a QR must never reach a non-genlock output.
    /// Static `include_str!` guard; fires red if the overlay leaks into it.
    #[test]
    fn legacy_pipeline_path_never_references_burn_overlay() {
        let src = include_str!("pipeline.rs");
        assert!(
            !src.contains("burn_overlay"),
            "the legacy pipeline path must never reference burn_overlay"
        );
        assert!(
            !src.contains("paint_burn"),
            "the legacy pipeline path must never call paint_burn"
        );
    }
}

// #147 standby same-path: the outer loop's standby black (legacy BGRA / paced
// no-op) + the cached paced NV12 black — an `impl` split for the 1000-line cap.
#[path = "submitter_standby.rs"]
mod submitter_standby;

#[cfg(test)]
#[path = "submitter_tests_timecode.rs"]
mod submitter_tests_timecode;

#[cfg(test)]
#[path = "submitter_tests_standby.rs"]
mod submitter_tests_standby;

#[cfg(test)]
#[path = "submitter_tests_mutants.rs"]
mod submitter_tests_mutants;
