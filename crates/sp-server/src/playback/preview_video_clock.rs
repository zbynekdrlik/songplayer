//! #221 — the live preview's VIDEO input on SongPlayer's monotonic clock, as a
//! pure, Linux-tested state machine, and the feeder thread that drives it
//! ([`spawn_video_feeder`], `mutants::skip` glue started by
//! `preview_encoder::run_child`): it only moves bytes, offering every tapped
//! canvas to [`VideoClock`] and writing whatever [`VideoClock::take_due`]
//! returns.
//!
//! Why: ffmpeg used to stamp the rawvideo input itself
//! (`-use_wallclock_as_timestamps`), with the SYSTEM wall clock, while the PCM
//! input counts samples that the audio feeder places on the monotonic clock
//! (`preview_audio_hold.rs`). The box's nightly UTC step (+1.543 s on
//! 6.10.2026 02:00:11) then moved the video timeline alone: a running encoder
//! showed the picture 3.5-4.7 s behind the sound for the rest of its life (local
//! repro), and a paused song kept showing new pictures for over 6 s (CI run
//! 37400177771; #221 comments 6008217701 and 6008231226).
//!
//! So the video input is COUNTED, like the audio: ffmpeg reads it at
//! `-framerate` [`PREVIEW_FPS`], and the feeder writes exactly that many frames
//! per second of the monotonic clock: frame `k` is due `k × FRAME_US` after the
//! first one, and it is the NEWEST tapped canvas at that moment, else the one
//! written before it again. A clock step moves neither input, and a pause freezes
//! the picture on the next slot while the stream keeps flowing. A canvas replaced
//! before its slot is skipped (the source runs faster than 25 fps); the feeder
//! hands it back to the tap's pool. After a write that blocked, every slot that
//! passed is written at once (the last canvas again), so the video timeline
//! never falls behind the audio's.
//!
//! Units: times are µs on the feeder's monotonic clock (`clock_base` in
//! `preview_encoder::run_child`, the same base the audio preroll reads).

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::info;

use super::preview_stream::StreamShared;

/// The preview's frame rate: the rawvideo input's `-framerate`, the output's
/// `-r` and its GOP `-g` (25 frames = one 1 s GOP = two 0.5 s fragments, the
/// lag beacon's `fmp4_relay::FRAGMENT_MS` invariant).
pub const PREVIEW_FPS: u64 = 25;

/// One frame slot at [`PREVIEW_FPS`] (µs). A literal, pinned against the rate
/// by a test.
pub const FRAME_US: u64 = 40_000;

/// How long the feeder waits for a canvas before the first one has arrived
/// (µs): it only bounds how often it checks for shutdown.
pub const IDLE_POLL_US: u64 = 200_000;

/// What the feeder has written so far, for its periodic log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoClockStats {
    /// Frames written to the encoder.
    pub written: u64,
    /// Writes of a canvas that had been written before (no newer one came:
    /// a pause, a decode stall, a catch-up after a blocked write).
    pub repeated: u64,
    /// Canvases replaced by a newer one before their slot came.
    pub skipped: u64,
    /// The most frames written at once (1 in steady state; more after a
    /// write that blocked).
    pub max_burst: u64,
}

/// The video feeder's frame schedule and current canvas (see the module doc).
#[derive(Debug)]
pub struct VideoClock<T> {
    /// When the first frame was due (µs); `None` until a canvas has arrived.
    start_us: Option<u64>,
    /// The canvas the next slot writes: the newest one offered.
    current: Option<T>,
    /// Whether `current` has been written at least once.
    current_written: bool,
    stats: VideoClockStats,
}

impl<T> Default for VideoClock<T> {
    fn default() -> Self {
        Self {
            start_us: None,
            current: None,
            current_written: false,
            stats: VideoClockStats::default(),
        }
    }
}

impl<T> VideoClock<T> {
    /// A clock with no canvas yet: nothing is due until one is offered.
    pub fn new() -> Self {
        Self::default()
    }

    /// A tapped canvas: the next slot writes it. Returns the canvas it
    /// replaces, for the tap's pool (counted as skipped if it was never
    /// written).
    pub fn offer(&mut self, frame: T) -> Option<T> {
        let old = self.current.replace(frame);
        if old.is_some() && !self.current_written {
            self.stats.skipped += 1;
        }
        self.current_written = false;
        old
    }

    /// The canvas to write at `now_us` and how many times: every slot due
    /// since the last call. The first call with a canvas starts the schedule
    /// at `now_us`, with that canvas as frame 0. `None` when no slot is due or
    /// no canvas has arrived yet.
    pub fn take_due(&mut self, now_us: u64) -> Option<(&T, u64)> {
        self.current.as_ref()?;
        let start = *self.start_us.get_or_insert(now_us);
        let due = now_us.saturating_sub(start) / FRAME_US + 1;
        let n = due.saturating_sub(self.stats.written);
        if n == 0 {
            return None;
        }
        self.stats.written += n;
        let fresh = u64::from(!self.current_written);
        self.stats.repeated += n - fresh;
        self.stats.max_burst = self.stats.max_burst.max(n);
        self.current_written = true;
        self.current.as_ref().map(|frame| (frame, n))
    }

    /// How long the feeder may wait for the next canvas at `now_us` (µs):
    /// until the next slot, 0 when a canvas waits for its first slot, and
    /// [`IDLE_POLL_US`] before any canvas.
    pub fn wait_us(&self, now_us: u64) -> u64 {
        match (self.start_us, &self.current) {
            (Some(start), _) => (start + self.stats.written * FRAME_US).saturating_sub(now_us),
            (None, Some(_)) => 0,
            (None, None) => IDLE_POLL_US,
        }
    }

    /// When frame 0 was due (µs), once the schedule started: the video
    /// timeline's origin, which the audio preroll aligns to.
    pub fn start_us(&self) -> Option<u64> {
        self.start_us
    }

    /// The counters for the feeder's log line.
    pub fn stats(&self) -> VideoClockStats {
        self.stats
    }
}

/// How often the video feeder logs its counters.
const VFEED_LOG_EVERY: Duration = Duration::from_secs(10);

/// Feed the child's video socket from the moment it connects (#178 r3) with
/// exactly [`PREVIEW_FPS`] frames per second of the monotonic `clock_base`
/// ([`VideoClock`]): each slot writes the newest tapped canvas, else the last
/// one again (a pause freezes the picture on the next slot), and every slot a
/// blocked write missed is written right after it. Runs until shutdown or a
/// write error (child gone). Drains any STALE queued frames on start (a
/// previous viewer's backlog would otherwise front-run the live edge), hands
/// every replaced canvas back to the tap's pool (#147 r10), and stores frame
/// 0's slot in `first_video_us` (0 = none yet): the video timeline's origin,
/// which the audio feeder's preroll aligns to.
#[cfg_attr(test, mutants::skip)]
pub(super) fn spawn_video_feeder(
    shared: Arc<StreamShared>,
    mut sock: TcpStream,
    shutdown: Arc<AtomicBool>,
    clock_base: Instant,
    first_video_us: Arc<AtomicU64>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("preview-vfeed".into())
        .spawn(move || {
            let rx = shared.video_receiver();
            while let Ok(stale) = rx.try_recv() {
                shared.recycle_frame(stale);
            }
            let us = || clock_base.elapsed().as_micros() as u64;
            let mut clock = VideoClock::new();
            let mut last_log = Instant::now();
            info!(
                stream = %shared.label(),
                fps = PREVIEW_FPS,
                "preview-vfeed: start — the video counts frames of the monotonic clock"
            );
            while !shutdown.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_micros(clock.wait_us(us()))) {
                    Ok(frame) => offer_canvas(&shared, &mut clock, frame),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
                // The newest canvas takes the slot.
                while let Ok(frame) = rx.try_recv() {
                    offer_canvas(&shared, &mut clock, frame);
                }
                if let Some((frame, n)) = clock.take_due(us()) {
                    if (0..n).any(|_| sock.write_all(frame).is_err()) {
                        break;
                    }
                    // `.max(1)`: a frame 0 at clock_base itself is not "none yet".
                    if first_video_us.load(Ordering::Relaxed) == 0
                        && let Some(start) = clock.start_us()
                    {
                        first_video_us.store(start.max(1), Ordering::Relaxed);
                    }
                }
                if last_log.elapsed() >= VFEED_LOG_EVERY {
                    let s = clock.stats();
                    info!(
                        stream = %shared.label(),
                        "preview-vfeed: written={} repeated={} skipped={} max_burst={} queued={}",
                        s.written, s.repeated, s.skipped, s.max_burst, rx.len()
                    );
                    last_log = Instant::now();
                }
            }
        })
}

/// Offer a tapped canvas to the video clock; the canvas it replaces goes back
/// to the tap's pool (#147 r10).
#[cfg_attr(test, mutants::skip)]
fn offer_canvas(shared: &StreamShared, clock: &mut VideoClock<Vec<u8>>, frame: Vec<u8>) {
    if let Some(old) = clock.offer(frame) {
        shared.recycle_frame(old);
    }
}

#[cfg(test)]
#[path = "preview_video_clock_tests.rs"]
mod tests;
