//! #221 — the live preview's VIDEO input on SongPlayer's monotonic clock, as a
//! pure, Linux-tested state machine, and the feeder thread that drives it
//! (`spawn_video_feeder`, `mutants::skip` glue started by
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
//! `-framerate` [`PREVIEW_FPS`], one frame per [`FRAME_US`] slot of the
//! monotonic clock from the first canvas on (#221 A1, ROZHODNUTÉ 6008679010):
//!
//! - A slot is written only with a NEW picture. Slot `k ≥ 1` is decided
//!   [`DECIDE_LATE_US`] (half a slot) after its time, and a canvas belongs to
//!   the first slot decided at or after its ARRIVAL; the newest canvas for a
//!   slot wins (the ones it replaced are skipped and go back to the tap's
//!   pool). So every slot shows the picture nearest its time, whatever the
//!   source's rate (24 fps: −18..+22 ms, 30 fps: −20..+7 ms).
//! - With no new picture nothing is written: during a pause the encoder
//!   starves and the picture holds pixel-exact. A repeated canvas would not:
//!   the encoder re-encodes it a little differently every few frames.
//! - The next new picture first fills every slot decided before it arrived
//!   with the LAST written picture (what ffmpeg's `cfr` did with the
//!   wall-clock stamps), then takes its own slot, so the video timeline counts
//!   every slot and stays on the audio's. A fill is bounded by
//!   [`MAX_GAP_FILL_SLOTS`]; a longer gap ends the encoder run instead (a new
//!   child starts both timelines at 0, its viewers reconnect onto its init).
//!
//! A clock step moves neither input. Units: times are µs on the feeder's
//! monotonic clock (`clock_base` in `preview_encoder::run_child`, the same base
//! the audio preroll reads).

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

/// How long after its time a slot `k ≥ 1` is decided (µs): half a slot, so a
/// slot gets the picture nearest its time, not one up to a whole slot old.
pub const DECIDE_LATE_US: u64 = 20_000;

/// The longest gap one new picture fills (slots): 10 s of 40 ms slots, at most
/// 250 canvases (~86 MB) written and encoded at once. It caps the catch-up
/// burst after a pause; a longer pause gets a fresh child when playback
/// resumes (the encoder's output had stalled since the pause anyway).
pub const MAX_GAP_FILL_SLOTS: u64 = 250;

/// How long the feeder waits for a canvas while none is pending (µs): it only
/// bounds how often it checks for shutdown (a canvas wakes it at once).
pub const IDLE_POLL_US: u64 = 200_000;

/// What the feeder has written, for its periodic log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoClockStats {
    /// Frames written to the encoder.
    pub written: u64,
    /// Slots filled with the last written picture after a gap (a pause, a
    /// decode stall, a write that blocked, a source below 25 fps).
    pub repeated: u64,
    /// Canvases replaced by a newer one before their slot came.
    pub skipped: u64,
    /// The largest gap fill since the last [`VideoClock::take_stats`] (1 in
    /// steady play: a new picture's own slot).
    pub max_burst: u64,
}

/// The video feeder's frame schedule (see the module doc).
#[derive(Debug)]
pub struct VideoClock<T> {
    /// When frame 0's canvas arrived (µs); `None` until one has.
    start_us: Option<u64>,
    /// The newest canvas not written yet, with its arrival (µs).
    pending: Option<(T, u64)>,
    /// The last canvas written: what a gap is filled with.
    last: Option<T>,
    /// The canvas the last write replaced as `last`, for the tap's pool.
    released: Option<T>,
    stats: VideoClockStats,
}

impl<T> Default for VideoClock<T> {
    fn default() -> Self {
        Self {
            start_us: None,
            pending: None,
            last: None,
            released: None,
            stats: VideoClockStats::default(),
        }
    }
}

/// Slots decided at or before `elapsed_us` after frame 0: frame 0 at once,
/// slot `k ≥ 1` at `k × FRAME_US + DECIDE_LATE_US`.
fn slots_due(elapsed_us: u64) -> u64 {
    elapsed_us.saturating_sub(DECIDE_LATE_US) / FRAME_US + 1
}

/// The slot of a canvas that arrived `elapsed_us` after frame 0: the first
/// one decided at or after its arrival (= the slots decided before it).
fn slot_of_arrival(elapsed_us: u64) -> u64 {
    match elapsed_us {
        0 => 0,
        _ => slots_due(elapsed_us - 1),
    }
}

impl<T> VideoClock<T> {
    /// A clock with no canvas yet: nothing is due until one is offered.
    pub fn new() -> Self {
        Self::default()
    }

    /// A tapped canvas that arrived at `arrival_us`: it takes its slot when
    /// that is decided. Returns the pending canvas it replaces (never
    /// written: skipped), for the tap's pool.
    pub fn offer(&mut self, frame: T, arrival_us: u64) -> Option<T> {
        let old = self
            .pending
            .replace((frame, arrival_us))
            .map(|(old, _)| old);
        if old.is_some() {
            self.stats.skipped += 1;
        }
        old
    }

    /// Frame 0's arrival and the pending canvas's slot (never one already
    /// written), once both exist.
    fn pending_slot(&self) -> Option<(u64, u64)> {
        let start = self.start_us?;
        let arrival = self.pending.as_ref()?.1;
        let slot = slot_of_arrival(arrival.saturating_sub(start)).max(self.stats.written);
        Some((start, slot))
    }

    /// The next write at `now_us`, if any: a canvas and how many times to
    /// write it. Call again until `None`: a gap first returns the last written
    /// picture for the slots decided before the new canvas arrived, then the
    /// new canvas once its own slot is decided. `None` while no new canvas is
    /// pending (a pause writes nothing), before its slot is decided, and when
    /// the gap is longer than [`MAX_GAP_FILL_SLOTS`] ([`Self::must_restart`]).
    /// The first canvas starts the schedule at its arrival as frame 0.
    pub fn take_due(&mut self, now_us: u64) -> Option<(&T, u64)> {
        // RED (#221): the 8d1f980a time-of-call schedule (a canvas takes the
        // slot due when it is TAKEN, not the one its arrival belongs to).
        self.pending.as_ref()?;
        let start = *self.start_us.get_or_insert(now_us);
        let due = slots_due(now_us.saturating_sub(start));
        let fill = due.checked_sub(self.stats.written + 1)?;
        if fill > MAX_GAP_FILL_SLOTS {
            return None;
        }
        if fill > 0 && self.last.is_some() {
            self.stats.written += fill;
            self.stats.repeated += fill;
            self.stats.max_burst = self.stats.max_burst.max(fill);
            return self.last.as_ref().map(|last| (last, fill));
        }
        let (fresh, _) = self.pending.take()?;
        self.stats.written = due;
        self.stats.max_burst = self.stats.max_burst.max(1);
        self.released = self.last.replace(fresh);
        self.last.as_ref().map(|frame| (frame, 1))
    }

    /// Whether the gap the pending canvas would fill is longer than
    /// [`MAX_GAP_FILL_SLOTS`]: the feeder ends the encoder run instead of
    /// writing it (a new child restarts both timelines).
    pub fn must_restart(&self) -> bool {
        self.pending_slot()
            .is_some_and(|(_, slot)| slot > self.stats.written + MAX_GAP_FILL_SLOTS)
    }

    /// The canvas the last write replaced as the fill picture, once (for the
    /// tap's pool).
    pub fn released(&mut self) -> Option<T> {
        self.released.take()
    }

    /// How long the feeder may wait for the next canvas at `now_us` (µs): 0
    /// for the first canvas or while a gap fill is due, until the pending
    /// canvas's slot is decided, and [`IDLE_POLL_US`] while none is pending.
    pub fn wait_us(&self, now_us: u64) -> u64 {
        // RED (#221): the 8d1f980a wait (to the next slot's decision).
        match (self.start_us, &self.pending) {
            (_, None) => IDLE_POLL_US,
            (None, Some(_)) => 0,
            (Some(start), Some(_)) => {
                (start + self.stats.written * FRAME_US + DECIDE_LATE_US).saturating_sub(now_us)
            }
        }
    }

    /// When frame 0's canvas arrived (µs), once the schedule started: the
    /// video timeline's origin, which the audio preroll aligns to.
    pub fn start_us(&self) -> Option<u64> {
        self.start_us
    }

    /// The counters, as they stand.
    pub fn stats(&self) -> VideoClockStats {
        self.stats
    }

    /// The counters for the feeder's log line; starts a new `max_burst`
    /// window.
    pub fn take_stats(&mut self) -> VideoClockStats {
        let stats = self.stats;
        self.stats.max_burst = 0;
        stats
    }
}

/// How often the video feeder logs its counters.
const VFEED_LOG_EVERY: Duration = Duration::from_secs(10);

/// Feed the child's video socket from the moment it connects (#178 r3) on the
/// [`VideoClock`] schedule of the monotonic `clock_base` (a canvas's arrival is
/// when the feeder receives it; a canvas wakes it at once): every new canvas
/// takes its 40 ms slot (after the gap's fill with the last picture), nothing
/// is written while none comes (a pause starves the encoder, the picture holds
/// pixel-exact). Runs until shutdown or a write error (child gone). A gap longer
/// than [`MAX_GAP_FILL_SLOTS`] sets `restart` and ends the feeder; the child's
/// monitor then ends the run, and the supervisor starts a fresh child for the
/// viewers. Drains any STALE queued frames on start (a previous viewer's backlog
/// would otherwise front-run the live edge), hands every replaced canvas back to
/// the tap's pool (#147 r10), and stores frame 0's arrival in `first_video_us`
/// (0 = none yet): the video timeline's origin, which the audio feeder's
/// preroll aligns to.
#[cfg_attr(test, mutants::skip)]
pub(super) fn spawn_video_feeder(
    shared: Arc<StreamShared>,
    mut sock: TcpStream,
    shutdown: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
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
            'feed: while !shutdown.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_micros(clock.wait_us(us()))) {
                    Ok(frame) => offer_canvas(&shared, &mut clock, frame, us()),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
                // The newest canvas takes the slot.
                while let Ok(frame) = rx.try_recv() {
                    offer_canvas(&shared, &mut clock, frame, us());
                }
                let now = us();
                if clock.must_restart() {
                    info!(
                        stream = %shared.label(),
                        max_fill_slots = MAX_GAP_FILL_SLOTS,
                        "preview-vfeed: a gap over 10 s — a fresh child restarts both timelines"
                    );
                    restart.store(true, Ordering::Relaxed);
                    break;
                }
                // At most the gap's fill, then the new canvas.
                for _ in 0..2 {
                    let Some((frame, n)) = clock.take_due(now) else {
                        break;
                    };
                    if (0..n).any(|_| sock.write_all(frame).is_err()) {
                        break 'feed;
                    }
                }
                if let Some(old) = clock.released() {
                    shared.recycle_frame(old);
                }
                // `.max(1)`: a frame 0 at clock_base itself is not "none yet".
                if first_video_us.load(Ordering::Relaxed) == 0
                    && let Some(start) = clock.start_us()
                {
                    first_video_us.store(start.max(1), Ordering::Relaxed);
                }
                if last_log.elapsed() >= VFEED_LOG_EVERY {
                    let s = clock.take_stats();
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

/// Offer a tapped canvas that the feeder received at `arrival_us` to the
/// video clock; a pending canvas it replaces goes back to the tap's pool
/// (#147 r10).
#[cfg_attr(test, mutants::skip)]
fn offer_canvas(
    shared: &StreamShared,
    clock: &mut VideoClock<Vec<u8>>,
    frame: Vec<u8>,
    arrival_us: u64,
) {
    if let Some(old) = clock.offer(frame, arrival_us) {
        shared.recycle_frame(old);
    }
}

#[cfg(test)]
#[path = "preview_video_clock_tests.rs"]
mod tests;
