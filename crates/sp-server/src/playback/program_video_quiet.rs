//! #251 step 4: the `SP-program` video side never starts inside Arena's
//! read window (design: #251 comment 6206228023).
//!
//! Resolume Arena reads Spout's shared texture about 8 ms after each vblank
//! of the primary display, and memory-bus load around that instant makes it
//! show its OLDER buffer for a refresh: a burst at +6 ms raised the wall's
//! jumps back 15–50× (SNV 16.10.2026, comment 6206181014). The
//! `SP-program` thread's video side — the 1920×1080 canvas fit (for a
//! 24/25 fps source the CPU blend of two native pictures), the NDI submit,
//! and the SDK's encode and cg OBS's decode right after it — starts at the
//! program's boundary, which drifts against the vblank. So a boundary whose
//! video side would start inside the quiet window `[from, to)` after the
//! vblank waits until `to` ([`video_start`]); VBAN already has its block
//! (`ProgramOutput::serve`), MAX its job. The window's settings are
//! `program_video_quiet_from_ms` / `_to_ms` (3–10.5 ms): at most 7.5 ms of
//! delay keeps camera-box's arrival-latched FIFO on cg OBS in its budget.
//!
//! The grid is the `program-max` thread's: it publishes the one it paced
//! MAX on at every boundary ([`VideoQuiet::publish_grid`]); a grid not read
//! within `sp_gpu::VBLANK_STALE` (MAX off, a dark output) means no wait.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use sp_core::config::{DEFAULT_PROGRAM_VIDEO_QUIET_FROM_US, DEFAULT_PROGRAM_VIDEO_QUIET_TO_US};
use sp_gpu::{VblankGrid, grid_is_fresh};

use super::program_max_send::SendClock;
use super::program_max_vblank::phase_after_vblank;

/// The quiet window after each vblank: the video side never starts in
/// `[from, to)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuietWindow {
    pub from: Duration,
    pub to: Duration,
}

/// When the video side of a boundary at `now` may start: `None` = at once
/// (no grid, a window ending past one refresh, or `now` outside it — a
/// window whose start is not before its end holds no instant, so it is
/// off); else the window's end in this refresh.
pub fn video_start(now: Instant, grid: Option<VblankGrid>, window: QuietWindow) -> Option<Instant> {
    let grid = grid?;
    if window.to > grid.period {
        return None;
    }
    let phase = phase_after_vblank(&grid, now);
    (phase >= window.from && phase < window.to).then(|| now + (window.to - phase))
}

#[derive(Debug)]
struct Inner {
    /// The grid the `program-max` thread last paced on, and when it read
    /// it; `None` when it paced on none.
    grid: Option<(VblankGrid, Instant)>,
    from_us: u64,
    to_us: u64,
    waits: u64,
    wait_us_max: u64,
}

/// The quiet window's state, shared by the `program-max` thread (the grid)
/// and the `SP-program` thread (the waits); one short lock each.
#[derive(Debug)]
pub struct VideoQuiet {
    inner: Mutex<Inner>,
}

impl Default for VideoQuiet {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                grid: None,
                from_us: DEFAULT_PROGRAM_VIDEO_QUIET_FROM_US,
                to_us: DEFAULT_PROGRAM_VIDEO_QUIET_TO_US,
                waits: 0,
                wait_us_max: 0,
            }),
        }
    }
}

/// `GET /api/v1/program` → `max.video_quiet`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VideoQuietStatus {
    /// The settings, as last applied, µs after the vblank.
    pub from_us: u64,
    pub to_us: u64,
    /// A fresh grid is there to wait on now.
    pub grid: bool,
    /// Boundaries whose video side waited, and the longest wait, µs, since
    /// the start.
    pub waits: u64,
    pub wait_us_max: u64,
}

impl VideoQuiet {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The `program-max` thread, at each boundary: the grid it paced on
    /// (`None`: none), read at `seen`.
    pub fn publish_grid(&self, grid: Option<VblankGrid>, seen: Instant) {
        self.lock().grid = grid.map(|grid| (grid, seen));
    }

    /// Apply the two settings, µs; whether they changed.
    pub fn set_window_us(&self, from_us: u64, to_us: u64) -> bool {
        let mut inner = self.lock();
        let changed = (inner.from_us, inner.to_us) != (from_us, to_us);
        inner.from_us = from_us;
        inner.to_us = to_us;
        changed
    }

    pub fn window(&self) -> QuietWindow {
        let inner = self.lock();
        QuietWindow {
            from: Duration::from_micros(inner.from_us),
            to: Duration::from_micros(inner.to_us),
        }
    }

    /// The grid read within `sp_gpu::VBLANK_STALE` of `now`, else none.
    fn fresh_grid(&self, now: Instant) -> Option<VblankGrid> {
        let (grid, seen) = self.lock().grid?;
        grid_is_fresh(seen, now).then_some(grid)
    }

    /// [`video_start`] on the fresh grid and the window.
    pub fn start_at(&self, now: Instant) -> Option<Instant> {
        video_start(now, self.fresh_grid(now), self.window())
    }

    pub fn record_wait(&self, waited: Duration) {
        let us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX);
        let mut inner = self.lock();
        inner.waits += 1;
        inner.wait_us_max = inner.wait_us_max.max(us);
    }

    pub fn status(&self, now: Instant) -> VideoQuietStatus {
        let grid = self.fresh_grid(now).is_some();
        let inner = self.lock();
        VideoQuietStatus {
            from_us: inner.from_us,
            to_us: inner.to_us,
            grid,
            waits: inner.waits,
            wait_us_max: inner.wait_us_max,
        }
    }
}

/// The `SP-program` thread, before a boundary's video side: wait on
/// `clock` while the window holds, record it; the wait planned, `None`
/// when none.
pub fn wait_out(quiet: &VideoQuiet, clock: &mut dyn SendClock) -> Option<Duration> {
    let now = clock.now();
    let at = quiet.start_at(now)?;
    clock.wait_until(at);
    let waited = at - now;
    quiet.record_wait(waited);
    Some(waited)
}

#[cfg(test)]
#[path = "program_video_quiet_tests.rs"]
mod tests;
