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
/// Saturating and guarded: a zero/sub-tick `elapsed` yields 0 (never a divide
/// hazard), and a decreasing counter (a wrapped or reset counter) yields 0
/// rather than a bogus spike.
pub fn faults_per_sec(prev: u64, now: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return 0;
    }
    let delta = now.saturating_sub(prev);
    (delta as f64 / secs / 2.0) as u64
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

    /// A wrapped / reset counter (now < prev) must read 0, never a huge spike.
    #[test]
    fn faults_per_sec_saturates_on_a_decreasing_counter() {
        assert_eq!(faults_per_sec(9_000, 1_000, Duration::from_secs(2)), 0);
    }

    #[test]
    fn fault_sample_interval_is_five_seconds() {
        assert_eq!(FAULT_SAMPLE_INTERVAL, Duration::from_secs(5));
    }
}
