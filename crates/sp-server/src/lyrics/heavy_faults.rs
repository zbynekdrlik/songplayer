//! #168 — page-fault-rate readout for the heavy child.
//!
//! The acceptance number for #168 is the separation child's page-fault rate
//! (target ≤ 20k/s with the retained mimalloc heap, down from ~193k/s). This
//! puts it in the log instead of a manual `Get-Counter`: a `#[cfg(windows)]`
//! sampler reads the child's cumulative `PageFaultCount` (`GetProcessMemoryInfo`)
//! every [`FAULT_SAMPLE_INTERVAL`] and logs the per-second delta. Because the
//! materialised venv interpreter (`bootstrap_venv_exe`) is now the REAL torch
//! process (not CPython's redirector), the counter reads the right process.
//!
//! The pure rate computation ([`faults_per_sec`]) is unit-tested on Linux; the
//! sampler that reads the OS counter is the Windows-only integration seam.

use std::time::Duration;

/// How often the sampler reads the child's cumulative page-fault counter.
pub const FAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Pure: page faults per second between two cumulative `PageFaultCount`
/// readings `prev` → `now` over `elapsed`.
///
/// A zero/sub-tick `elapsed` yields 0 (never a divide hazard). The counter is
/// the **u32** `PROCESS_MEMORY_COUNTERS.PageFaultCount`, which WRAPS (the child
/// ran ~136–193k faults/s, #168 — a wrap every ~6–9 h) and never resets within
/// a process, so a decrease is a wrap: the delta is the shared wrap-safe
/// `process_start::residency::fault_delta` (#147 round 9 — it used to
/// `saturating_sub` a decrease to 0, which reported a spurious 0 faults/s at
/// every wrap).
pub fn faults_per_sec(prev: u32, now: u32, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0;
    }
    let delta = crate::process_start::residency::fault_delta(prev, now);
    (delta as f64 / secs) as u64
}

/// Spawn a background task that samples the child `pid`'s cumulative
/// `PageFaultCount` every [`FAULT_SAMPLE_INTERVAL`] and logs the per-second
/// delta at INFO (`heavy child faults/s=N`). The returned [`AbortHandle`] stops
/// it — the job guard aborts it when the heavy child exits. Sampling ends by
/// itself once the process can no longer be read (it exited).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
pub fn spawn_fault_sampler(pid: u32) -> tokio::task::AbortHandle {
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(FAULT_SAMPLE_INTERVAL);
        ticker.tick().await; // consume the immediate first tick
        let mut prev: Option<u32> = None;
        let mut last = std::time::Instant::now();
        loop {
            ticker.tick().await;
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last);
            last = now;
            match read_page_fault_count(pid) {
                Some(count) => {
                    if let Some(p) = prev {
                        let rate = faults_per_sec(p, count, elapsed);
                        tracing::info!("heavy child faults/s={rate} (pid {pid})");
                    }
                    prev = Some(count);
                }
                // Process gone / unreadable — stop sampling.
                None => break,
            }
        }
    });
    task.abort_handle()
}

/// Read a process's cumulative `PageFaultCount` via `GetProcessMemoryInfo`.
/// `None` if the process cannot be opened or queried (e.g. it already exited).
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn read_page_fault_count(pid: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    // SAFETY: the process is opened for query only; the handle is closed on
    // every path; `counters` is a correctly-sized, zero-initialised POD that
    // GetProcessMemoryInfo only writes into (returns 0 on failure).
    unsafe {
        let proc: HANDLE = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
        if proc.is_null() {
            return None;
        }
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = GetProcessMemoryInfo(proc, &mut counters, counters.cb);
        CloseHandle(proc);
        if ok == 0 {
            return None;
        }
        Some(counters.PageFaultCount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_per_sec_divides_delta_by_elapsed_seconds() {
        assert_eq!(faults_per_sec(1_000, 4_000, Duration::from_secs(3)), 1_000);
    }

    #[test]
    fn faults_per_sec_zero_elapsed_is_zero_not_a_panic() {
        assert_eq!(faults_per_sec(1_000, 4_000, Duration::ZERO), 0);
    }

    /// #147 round 9: `PageFaultCount` is a u32 that WRAPS (it never resets
    /// within a process), so now < prev is a wrap — counted across u32::MAX,
    /// not zeroed. `4_294_966_296` = `u32::MAX - 999`: 999 + 1 + 1 000 = 2 000
    /// faults over 2 s → 1 000/s.
    #[test]
    fn faults_per_sec_counts_across_a_u32_wrap() {
        assert_eq!(
            faults_per_sec(4_294_966_296, 1_000, Duration::from_secs(2)),
            1_000
        );
    }

    #[test]
    fn fault_sample_interval_is_five_seconds() {
        assert_eq!(FAULT_SAMPLE_INTERVAL, Duration::from_secs(5));
    }
}
