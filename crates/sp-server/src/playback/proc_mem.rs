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
//! process-wide cumulative **u32** that wraps (≈ every 2.4 h at the box's
//! measured rate); the wrap-safe delta is the shared pure
//! `process_start::residency::fault_delta`. The counter is process-wide, not per
//! output, so ONE process-global [`FaultWindow`] samples it at most once per
//! [`SAMPLE_PERIOD_MS`] (driven by whichever paced heartbeat comes first) and
//! every paced output's line carries the same last-full-minute value.
//!
//! The paced heartbeat runs on the boundary-critical EMIT thread, so the
//! Windows [`gauge`] never blocks: it only `try_lock`s the window (a busy lock
//! means another output is sampling right now — skip), and every caller reads
//! the published gauge from two lock-free atomics.
//!
//! The window arithmetic + the atomic slot encoding are pure + Linux-tested +
//! mutation-scored; only [`gauge`]'s OS read is `#[cfg(windows)]`
//! (`mutants::skip`, box-verified).

use crate::process_start::residency::{bytes_to_mb, fault_delta};

/// The sampling period: one full minute, matching the once-per-UTC-minute
/// `pipeline: loop-stats` cadence.
pub const SAMPLE_PERIOD_MS: u64 = 60_000;

/// A gap longer than this between two readings (no paced heartbeat ran — e.g.
/// every pipeline was torn down) re-baselines instead of computing a rate: over
/// a long enough gap the u32 counter could wrap more than once, and one
/// averaged rate over minutes of silence is not the "last minute" anyway.
pub const MAX_SAMPLE_GAP_MS: u64 = 300_000;

/// One full-minute reading of SongPlayer's memory residency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcMemGauge {
    /// `PageFaultCount` delta over the last sample window, normalised to one
    /// minute (soft + hard faults, the counter Windows exposes).
    pub page_faults_per_min: u64,
    /// `WorkingSetSize` at the end of that window, in MiB.
    pub working_set_mb: u64,
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
    /// last one is ignored (the previous gauge stands); a reading more than
    /// [`MAX_SAMPLE_GAP_MS`] after it re-baselines (gauge back to `None`);
    /// otherwise the gauge is recomputed over the actual elapsed window. The
    /// reading then becomes the new baseline. Returns the current gauge. Pure.
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
            self.latest = if elapsed > MAX_SAMPLE_GAP_MS {
                None
            } else {
                Some(ProcMemGauge {
                    page_faults_per_min: per_minute(fault_delta(prev, page_fault_count), elapsed),
                    working_set_mb: bytes_to_mb(working_set_bytes),
                })
            };
        }
        self.last = Some((page_fault_count, now_ms));
        self.latest
    }

    /// The last full-minute gauge (`None` until two readings a minute apart).
    pub fn latest(&self) -> Option<ProcMemGauge> {
        self.latest
    }
}

/// The "no gauge" sentinel stored in the lock-free slots.
pub const NO_READING: u64 = u64::MAX;

/// Encode a gauge into the two lock-free slot values; `None` → both
/// [`NO_READING`]. A real value is capped one below the sentinel so it can
/// never read back as "no reading". Pure.
pub fn to_slots(g: Option<ProcMemGauge>) -> (u64, u64) {
    match g {
        Some(g) => (
            g.page_faults_per_min.min(NO_READING - 1),
            g.working_set_mb.min(NO_READING - 1),
        ),
        None => (NO_READING, NO_READING),
    }
}

/// Decode the two slot values; either slot at [`NO_READING`] → `None`. Pure.
pub fn from_slots(faults: u64, working_set_mb: u64) -> Option<ProcMemGauge> {
    if faults == NO_READING || working_set_mb == NO_READING {
        return None;
    }
    Some(ProcMemGauge {
        page_faults_per_min: faults,
        working_set_mb,
    })
}

/// Render an optional gauge value for the log: the number, or `na` before the
/// first full minute / off Windows. Pure.
pub fn fmt_opt(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "na".to_string(),
    }
}

/// The process-global window the paced heartbeats share (Windows only). A
/// struct literal (not a `const fn new`) so there is no trivial constructor
/// body for the mutation gate to swap for `Default::default()`.
#[cfg(windows)]
static WINDOW: std::sync::Mutex<FaultWindow> = std::sync::Mutex::new(FaultWindow {
    last: None,
    latest: None,
});

/// The published gauge, readable without the lock (Windows only). Two relaxed
/// atomics: a reader racing a publish can pair one new and one old field for
/// one heartbeat — harmless for a per-minute telemetry line.
#[cfg(windows)]
static LATEST_FAULTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(NO_READING);
#[cfg(windows)]
static LATEST_WS_MB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(NO_READING);

/// The monotonic origin for the window's millisecond clock (Windows only).
#[cfg(windows)]
static T0: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// SongPlayer's current full-minute memory gauge, sampling the OS at most once
/// per [`SAMPLE_PERIOD_MS`]. Called from the paced heartbeat (≈ every 5 s per
/// output, on the EMIT thread): it never blocks — `try_lock` only (a busy
/// window means another output is sampling; this call just reads the published
/// value), and the OS read happens ~once a minute process-wide, never per frame.
///
/// mutants::skip — an OS read + process-globals; the arithmetic it drives is
/// the pure, tested [`FaultWindow`] + [`to_slots`] / [`from_slots`].
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn gauge() -> Option<ProcMemGauge> {
    use std::sync::atomic::Ordering::Relaxed;
    let now_ms = T0.elapsed().as_millis() as u64;
    if let Ok(mut w) = WINDOW.try_lock()
        && w.due(now_ms)
        && let Some((count, ws)) = read_own_memory_counters()
    {
        let (faults, ws_mb) = to_slots(w.observe(count, ws, now_ms));
        LATEST_FAULTS.store(faults, Relaxed);
        LATEST_WS_MB.store(ws_mb, Relaxed);
    }
    from_slots(LATEST_FAULTS.load(Relaxed), LATEST_WS_MB.load(Relaxed))
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
