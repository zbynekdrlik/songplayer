//! #147 round 9 — SongPlayer's OWN page-fault rate + working set, carried on
//! the paced `pipeline: loop-stats` line beside `submit_call_us_max`.
//!
//! The round-9 design (issue #147 comment 5812936370) attributes the residual
//! paced-sender stall with a heavy child resident to memory residency: when the
//! child's working set grows, Windows trims SongPlayer's frame pools / NDI SDK
//! buffers and the paced submit takes hard page faults. So the per-minute line
//! must show SongPlayer's page faults per minute and its resident working set
//! next to the submit-call cost; the box A/B windows then read the correlation
//! straight from the log (and confirm the hard minimum working set holds).
//!
//! `PageFaultCount` (`PROCESS_MEMORY_COUNTERS`, `GetProcessMemoryInfo`) is a
//! process-wide cumulative **u32**. At the 430–500k faults/s the box measured
//! on the paced path (#203) it WRAPS roughly every 2.4 h, so the delta is taken
//! with `wrapping_sub` — a process's counter never resets, so a decrease can
//! only be a 32-bit wrap. The counter is process-wide, not per output, so ONE
//! process-global [`FaultWindow`] samples it at most once per
//! [`SAMPLE_PERIOD_MS`] (driven by whichever paced heartbeat comes first) and
//! every paced output's line carries the same last-full-minute value.
//!
//! The window arithmetic is pure + Linux-tested + mutation-scored; only
//! [`gauge`]'s OS read is `#[cfg(windows)]` (`mutants::skip`, box-verified).

use crate::process_start::residency::bytes_to_mb;

/// The sampling period: one full minute, matching the once-per-UTC-minute
/// `pipeline: loop-stats` cadence.
pub const SAMPLE_PERIOD_MS: u64 = 60_000;

/// One full-minute reading of SongPlayer's memory residency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcMemGauge {
    /// `PageFaultCount` delta over the last sample window, normalised to one
    /// minute (soft + hard faults, the counter Windows exposes).
    pub page_faults_per_min: u64,
    /// `WorkingSetSize` at the end of that window, in MiB.
    pub working_set_mb: u64,
}

/// Faults between two cumulative `PageFaultCount` readings. Wrap-safe: the u32
/// counter wraps (it never resets within a process), so `now < prev` means one
/// wrap, and `wrapping_sub` yields the true forward distance. Pure.
pub fn fault_delta(prev: u32, now: u32) -> u64 {
    now.wrapping_sub(prev) as u64
}

/// Normalise a fault `delta` observed over `elapsed_ms` to faults per minute
/// (`delta × 60 000 / elapsed_ms`, integer). A zero window yields 0, never a
/// divide-by-zero. Pure.
pub fn per_minute(delta: u64, elapsed_ms: u64) -> u64 {
    if elapsed_ms == 0 {
        return 0;
    }
    delta.saturating_mul(60_000) / elapsed_ms
}

/// The process-wide sampling state: the last `(PageFaultCount, t_ms)` reading
/// and the last full-minute gauge. `t_ms` is a monotonic millisecond clock
/// supplied by the caller (so the arithmetic stays pure and testable).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FaultWindow {
    last: Option<(u32, u64)>,
    latest: Option<ProcMemGauge>,
}

impl FaultWindow {
    /// An empty window (no reading yet). `const` so it can seed a `static`.
    pub const fn new() -> Self {
        Self {
            last: None,
            latest: None,
        }
    }

    /// Whether a new OS reading is due at `now_ms`: always before the first
    /// reading, then once at least [`SAMPLE_PERIOD_MS`] has elapsed. A clock
    /// that went backwards is "not due" (saturating), never a bogus sample.
    pub fn due(&self, now_ms: u64) -> bool {
        match self.last {
            None => true,
            Some((_, t)) => now_ms.saturating_sub(t) >= SAMPLE_PERIOD_MS,
        }
    }

    /// Feed one reading. The FIRST reading only arms the window (no delta
    /// exists yet → `None`); a reading less than [`SAMPLE_PERIOD_MS`] after the
    /// last one is ignored (the previous gauge stands); otherwise the gauge is
    /// recomputed over the actual elapsed window and the reading becomes the new
    /// baseline. Returns the current gauge. Pure.
    pub fn observe(
        &mut self,
        page_fault_count: u32,
        working_set_bytes: u64,
        now_ms: u64,
    ) -> Option<ProcMemGauge> {
        if let Some((prev, t)) = self.last {
            let elapsed = now_ms.saturating_sub(t);
            if elapsed < SAMPLE_PERIOD_MS {
                return self.latest;
            }
            self.latest = Some(ProcMemGauge {
                page_faults_per_min: per_minute(fault_delta(prev, page_fault_count), elapsed),
                working_set_mb: bytes_to_mb(working_set_bytes),
            });
        }
        self.last = Some((page_fault_count, now_ms));
        self.latest
    }

    /// The last full-minute gauge (`None` until two readings a minute apart).
    pub fn latest(&self) -> Option<ProcMemGauge> {
        self.latest
    }
}

/// Render an optional gauge value for the log: the number, or `na` before the
/// first full minute / off Windows. Pure.
pub fn fmt_opt(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "na".to_string(),
    }
}

/// The process-global window the paced heartbeats share (Windows only).
#[cfg(windows)]
static WINDOW: std::sync::Mutex<FaultWindow> = std::sync::Mutex::new(FaultWindow::new());

/// The monotonic origin for the window's millisecond clock (Windows only).
#[cfg(windows)]
static T0: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// SongPlayer's current full-minute memory gauge, sampling the OS at most once
/// per [`SAMPLE_PERIOD_MS`]. Called from the paced heartbeat (≈ every 5 s per
/// output), so the OS read is ~once a minute process-wide, never per frame.
///
/// mutants::skip — an OS read + a process-global; the arithmetic it drives is
/// the pure, tested [`FaultWindow`].
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn gauge() -> Option<ProcMemGauge> {
    let now_ms = T0.elapsed().as_millis() as u64;
    let mut w = WINDOW.lock().ok()?;
    if w.due(now_ms) {
        if let Some((count, ws)) = read_own_memory_counters() {
            return w.observe(count, ws, now_ms);
        }
    }
    w.latest()
}

/// Non-Windows: no process memory counters to read (prod is Windows-only).
///
/// mutants::skip — a constant `None` (its only mutant is itself).
#[cfg(not(windows))]
#[cfg_attr(test, mutants::skip)]
pub fn gauge() -> Option<ProcMemGauge> {
    None
}

/// Read SongPlayer's own `(PageFaultCount, WorkingSetSize)` via
/// `GetProcessMemoryInfo` on the current-process pseudo-handle. `None` on
/// failure. Integration-only.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn read_own_memory_counters() -> Option<(u32, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: GetCurrentProcess is the current-process pseudo-handle (never
    // closed, full access); `counters` is a correctly sized, zero-initialised
    // POD that GetProcessMemoryInfo only writes into (returns 0 on failure).
    unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) == 0 {
            return None;
        }
        Some((counters.PageFaultCount, counters.WorkingSetSize as u64))
    }
}

#[cfg(test)]
#[path = "proc_mem_tests.rs"]
mod tests;
