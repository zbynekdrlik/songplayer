//! #251 step 5: a heavy child never loads the memory bus while Arena reads
//! Spout (design: #251 comment 6207492230).
//!
//! A heavy child (the stem separation, the vocal isolation, mtl, the dub)
//! running on the CPU while the wall plays loads the memory bus all the
//! time, so also in Arena's read window, ~6–10 ms after each vblank of the
//! primary display: the wall then shows the older picture 8–18× more often
//! (#251 comments 6201607634, 6206181014). Step 4 keeps SongPlayer's own
//! per-boundary work out of that window; a continuous process needs pacing.
//! So a paced child is suspended for the window `[from, to)` of every
//! refresh — the same window and grid as the `SP-program` video side
//! (`playback::program_video_quiet`) — and runs the rest: [`quiet_step`]
//! decides, the Windows glue ([`QuietPacer`]) acts. Only a CPU child is
//! paced (`HeavyStepPlan::paced`): a GPU child may hold a driver lock Arena
//! needs. With no fresh grid (MAX off, a dark output) the child runs. Only
//! a child under its Job Object is paced (kill-on-close: a SongPlayer that
//! dies while it is held takes it along). Known limit: only the child's own
//! process is held, not a grandchild it starts (the dub child's ffmpeg
//! decode at its start).

use std::time::{Duration, Instant};

use sp_gpu::VblankGrid;

use crate::playback::program_max_vblank::phase_after_vblank;
use crate::playback::program_video_quiet::QuietWindow;

/// How long the pacer runs a child with no grid before it looks again.
pub const NO_GRID_RECHECK: Duration = Duration::from_millis(100);

/// What the pacer does now, and until when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuietStep {
    /// Let the child run until the window's next start.
    Run { until: Instant },
    /// Hold the child until the window's end.
    Pause { until: Instant },
    /// No fresh grid, or a window that ends past one refresh: let it run
    /// and look again in [`NO_GRID_RECHECK`].
    NoGrid,
}

/// The pacer's step at `now` on `grid` for `window`: inside `[from, to)`
/// after the vblank, pause until `to`; before it, run until `from`; after
/// it, run until the next refresh's `from`. A window whose start is not
/// before its end is never entered (the child just runs).
pub fn quiet_step(now: Instant, grid: Option<VblankGrid>, window: QuietWindow) -> QuietStep {
    let Some(grid) = grid else {
        return QuietStep::NoGrid;
    };
    if window.to > grid.period {
        return QuietStep::NoGrid;
    }
    let phase = phase_after_vblank(&grid, now);
    if phase < window.from {
        QuietStep::Run {
            until: now + (window.from - phase),
        }
    } else if phase < window.to {
        QuietStep::Pause {
            until: now + (window.to - phase),
        }
    } else {
        QuietStep::Run {
            until: now + (grid.period - phase) + window.from,
        }
    }
}

#[cfg(windows)]
pub use win::QuietPacer;

/// Off Windows nothing is paced.
#[cfg(not(windows))]
pub struct QuietPacer;

#[cfg(not(windows))]
impl QuietPacer {
    #[cfg_attr(test, mutants::skip)]
    pub fn start(_pid: u32) -> Option<Self> {
        None
    }
}

#[cfg(windows)]
mod win {
    //! The glue: suspend and resume the child by `NtSuspendProcess` /
    //! `NtResumeProcess` (ntdll; the windows crates have no binding), as
    //! `sp-gpu`'s `win/spout_lock.rs` links `NtQueryMutant`. The thread waits
    //! on [stop event, the child] with the step's timeout (rounded up to the
    //! ms: the process runs the 1 ms timer), so a stop or the child's exit
    //! ends it at once, and [`Held`] resumes the child whatever ends it.

    use std::thread::JoinHandle;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, OpenProcess, PROCESS_SUSPEND_RESUME, PROCESS_SYNCHRONIZE, SetEvent,
        WaitForMultipleObjects,
    };

    use super::{NO_GRID_RECHECK, QuietStep, quiet_step};

    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtSuspendProcess(process: HANDLE) -> i32;
        fn NtResumeProcess(process: HANDLE) -> i32;
    }

    /// A thread that holds one child while Arena reads; dropping it sets the
    /// stop event and joins the thread, which resumes the child first.
    pub struct QuietPacer {
        /// The manual-reset stop event, kept as a number (the guard is `Send`).
        stop: isize,
        thread: Option<JoinHandle<()>>,
    }

    impl QuietPacer {
        /// Start pacing process `pid`; `None` (WARNed) when it cannot be
        /// opened or the thread not started: the child then just runs.
        #[cfg_attr(test, mutants::skip)]
        pub fn start(pid: u32) -> Option<Self> {
            // SAFETY: plain kernel object creation; both handles are closed
            // below on failure, else by the thread (child) and Drop (stop).
            let child =
                unsafe { OpenProcess(PROCESS_SUSPEND_RESUME | PROCESS_SYNCHRONIZE, 0, pid) };
            if child.is_null() {
                tracing::warn!(
                    pid,
                    "heavy child: cannot open it to pace it - it runs unpaced"
                );
                return None;
            }
            let stop = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if stop.is_null() {
                tracing::warn!(
                    pid,
                    "heavy child: no stop event for its pacer - it runs unpaced"
                );
                unsafe { CloseHandle(child) };
                return None;
            }
            let (child_raw, stop_raw) = (child as isize, stop as isize);
            let thread = std::thread::Builder::new()
                .name(format!("heavy-quiet-{pid}"))
                .spawn(move || run(child_raw as HANDLE, stop_raw as HANDLE, pid));
            match thread {
                Ok(thread) => Some(Self {
                    stop: stop_raw,
                    thread: Some(thread),
                }),
                Err(e) => {
                    tracing::warn!(pid, %e, "heavy child: the pacer did not start - it runs unpaced");
                    unsafe {
                        CloseHandle(child);
                        CloseHandle(stop);
                    }
                    None
                }
            }
        }
    }

    impl Drop for QuietPacer {
        #[cfg_attr(test, mutants::skip)]
        fn drop(&mut self) {
            // SAFETY: our own event, closed only here, after the join.
            unsafe { SetEvent(self.stop as HANDLE) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            unsafe { CloseHandle(self.stop as HANDLE) };
        }
    }

    /// The child's held state: resumed on drop, a panic included.
    struct Held {
        child: HANDLE,
        pid: u32,
        paused: bool,
        pauses: u64,
        suspend_failed: bool,
        resume_failed: bool,
    }

    impl Held {
        #[cfg_attr(test, mutants::skip)]
        fn pause(&mut self) {
            if self.paused {
                return;
            }
            // SAFETY: `child` carries PROCESS_SUSPEND_RESUME, open while held.
            let status = unsafe { NtSuspendProcess(self.child) };
            if status >= 0 {
                self.paused = true;
                self.pauses += 1;
            } else if !self.suspend_failed {
                self.suspend_failed = true;
                tracing::warn!(
                    pid = self.pid,
                    status,
                    "heavy child: suspending it failed - it runs"
                );
            }
        }

        /// Resume; a failure keeps it held (retried at the next step) with
        /// one ERROR.
        #[cfg_attr(test, mutants::skip)]
        fn resume(&mut self) {
            if !self.paused {
                return;
            }
            // SAFETY: as in `pause`.
            let status = unsafe { NtResumeProcess(self.child) };
            if status >= 0 {
                self.paused = false;
            } else if !self.resume_failed {
                self.resume_failed = true;
                tracing::error!(
                    pid = self.pid,
                    status,
                    "heavy child: resuming it failed - retrying"
                );
            }
        }
    }

    impl Drop for Held {
        #[cfg_attr(test, mutants::skip)]
        fn drop(&mut self) {
            self.resume();
        }
    }

    /// The pacer loop, until the stop event or the child's exit.
    #[cfg_attr(test, mutants::skip)]
    fn run(child: HANDLE, stop: HANDLE, pid: u32) {
        tracing::info!(
            pid,
            "heavy child: held while Arena reads Spout (wall in use)"
        );
        let mut held = Held {
            child,
            pid,
            paused: false,
            pauses: 0,
            suspend_failed: false,
            resume_failed: false,
        };
        let handles = [stop, child];
        loop {
            let now = Instant::now();
            let (grid, window) = match crate::playback::program_bus::installed() {
                Some(bus) => (bus.max().video.fresh_grid(now), bus.max().video.window()),
                None => (
                    None,
                    crate::playback::program_video_quiet::VideoQuiet::default().window(),
                ),
            };
            let wait = match quiet_step(now, grid, window) {
                QuietStep::Pause { until } => {
                    held.pause();
                    until.saturating_duration_since(Instant::now())
                }
                QuietStep::Run { until } => {
                    held.resume();
                    until.saturating_duration_since(Instant::now())
                }
                QuietStep::NoGrid => {
                    held.resume();
                    NO_GRID_RECHECK
                }
            };
            let ms = u32::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(u32::MAX - 1);
            // SAFETY: both handles stay open for the loop's life.
            let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, ms) };
            if woke != WAIT_TIMEOUT {
                break; // the stop event, the child's exit, or a failed wait
            }
        }
        held.resume();
        tracing::info!(pid, pauses = held.pauses, "heavy child: pacing ended");
        drop(held);
        // SAFETY: the process handle opened in `start`, no longer used.
        unsafe { CloseHandle(child) };
    }
}

#[cfg(test)]
#[path = "heavy_quiet_tests.rs"]
mod tests;
