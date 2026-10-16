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
//! needs. With no fresh grid (MAX off, a dark output) the child runs.

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
    //! `sp-gpu`'s `win/spout_lock.rs` links `NtQueryMutant`.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Instant;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SUSPEND_RESUME, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    use super::{NO_GRID_RECHECK, QuietStep, quiet_step};

    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn NtSuspendProcess(process: HANDLE) -> i32;
        fn NtResumeProcess(process: HANDLE) -> i32;
    }

    /// A thread that pauses one child while Arena reads; dropping it stops
    /// the thread, which resumes the child first.
    pub struct QuietPacer {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl QuietPacer {
        /// Start pacing process `pid`; `None` (WARNed) when it cannot be
        /// opened or the thread not started: the child then just runs.
        #[cfg_attr(test, mutants::skip)]
        pub fn start(pid: u32) -> Option<Self> {
            // SAFETY: a plain OpenProcess; the handle is closed by the thread.
            let handle =
                unsafe { OpenProcess(PROCESS_SUSPEND_RESUME | PROCESS_SYNCHRONIZE, 0, pid) };
            if handle.is_null() {
                tracing::warn!(
                    pid,
                    "heavy child: cannot open it to pace it - it runs unpaced"
                );
                return None;
            }
            let raw = handle as isize;
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let thread = std::thread::Builder::new()
                .name(format!("heavy-quiet-{pid}"))
                .spawn(move || run(raw as HANDLE, pid, &flag));
            match thread {
                Ok(thread) => Some(Self {
                    stop,
                    thread: Some(thread),
                }),
                Err(e) => {
                    tracing::warn!(pid, %e, "heavy child: the pacer did not start - it runs unpaced");
                    // SAFETY: the handle opened above, not handed to a thread.
                    unsafe { CloseHandle(raw as HANDLE) };
                    None
                }
            }
        }
    }

    impl Drop for QuietPacer {
        #[cfg_attr(test, mutants::skip)]
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The pacer loop: until the child exits or the guard stops it; the
    /// child is always resumed before the end.
    #[cfg_attr(test, mutants::skip)]
    fn run(child: HANDLE, pid: u32, stop: &AtomicBool) {
        tracing::info!(
            pid,
            "heavy child: paused while Arena reads Spout (wall in use)"
        );
        let mut paused = false;
        let mut pauses: u64 = 0;
        // SAFETY (every call below): `child` is our own process handle with
        // SUSPEND_RESUME | SYNCHRONIZE, open until the CloseHandle at the end.
        while !stop.load(Ordering::Acquire)
            && unsafe { WaitForSingleObject(child, 0) } != WAIT_OBJECT_0
        {
            let now = Instant::now();
            let (grid, window) = match crate::playback::program_bus::installed() {
                Some(bus) => (bus.max().video.fresh_grid(now), bus.max().video.window()),
                None => (
                    None,
                    crate::playback::program_video_quiet::VideoQuiet::default().window(),
                ),
            };
            match quiet_step(now, grid, window) {
                QuietStep::Pause { until } => {
                    if !paused {
                        unsafe { NtSuspendProcess(child) };
                        paused = true;
                        pauses += 1;
                    }
                    std::thread::sleep(until.saturating_duration_since(Instant::now()));
                }
                QuietStep::Run { until } => {
                    if paused {
                        unsafe { NtResumeProcess(child) };
                        paused = false;
                    }
                    std::thread::sleep(until.saturating_duration_since(Instant::now()));
                }
                QuietStep::NoGrid => {
                    if paused {
                        unsafe { NtResumeProcess(child) };
                        paused = false;
                    }
                    std::thread::sleep(NO_GRID_RECHECK);
                }
            }
        }
        if paused {
            unsafe { NtResumeProcess(child) };
        }
        unsafe { CloseHandle(child) };
        tracing::info!(pid, pauses, "heavy child: pacing ended");
    }
}

#[cfg(test)]
#[path = "heavy_quiet_tests.rs"]
mod tests;
