//! #207 — box-wide commit / commit-over-RAM observability.
//!
//! The `0xc0000409` aborts (#156 dump) are host allocation failures under
//! commit pressure: the box lives just under its commit limit (59 528 MB of
//! 63 173 MB on 23.9.), and nothing logs the slide, so an allocation failure is
//! the first visible symptom. This module adds a per-minute grep-stable host
//! line beside `pipeline: loop-stats` and the same numbers in
//! `GET /api/v1/status`, so the commit trend is readable without PowerShell.
//!
//! The [`HostCommit`] snapshot + the pure [`format_host_commit_line`] formatter
//! are cross-platform and Linux-tested; only the real read ([`read_host_commit`])
//! and the interval logger ([`run_host_commit_logger`]) touch Win32
//! (`GlobalMemoryStatusEx` + `GetPerformanceInfo`), returning `None` / logging
//! nothing off Windows.

use serde::{Deserialize, Serialize};

/// Bytes per mebibyte — the divisor turning a raw byte figure into the integer
/// MB the log line + status field report (a MiB, matching Task Manager's "MB").
const BYTES_PER_MB: u64 = 1_048_576;

/// One byte figure as integer MB (`bytes / 1_048_576`). Pure — shared by the log
/// line and the status field so both round identically.
pub fn to_mb(bytes: u64) -> u64 {
    bytes / BYTES_PER_MB
}

/// A box-wide commit / physical-memory snapshot, all in BYTES.
///
/// Read once a minute (Windows) via `GlobalMemoryStatusEx`
/// (`ullTotalPageFile` = commit limit, `ullAvailPageFile` = free commit,
/// `ullTotalPhys`, `ullAvailPhys`) + `GetPerformanceInfo`
/// (`CommitTotal * PageSize` = committed bytes). `commit_over_ram` has no direct
/// field, so it is `committed − (total_phys − free_phys)` clamped at 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostCommit {
    /// Total committed bytes across the box (`CommitTotal * PageSize`).
    pub committed_bytes: u64,
    /// The box's commit limit (`ullTotalPageFile`).
    pub commit_limit_bytes: u64,
    /// Free commit remaining before the limit (`ullAvailPageFile`).
    pub free_commit_bytes: u64,
    /// Free physical RAM (`ullAvailPhys`).
    pub free_phys_bytes: u64,
    /// Total physical RAM (`ullTotalPhys`) — used to derive `commit_over_ram`.
    pub total_phys_bytes: u64,
    /// #207: committed bytes NOT backed by resident RAM — `committed −
    /// (total_phys − free_phys)`, clamped at 0. This is "commit beyond RAM"
    /// (pagefile-backed sections / reserved-committed arenas), NOT pagefile
    /// usage: on the box it read 48.5 GB derived vs 33.4 GB real
    /// `Win32_PageFileUsage`, so the honest name is `commit_over_ram`.
    pub commit_over_ram_bytes: u64,
}

/// The grep-stable per-minute host line logged beside `pipeline: loop-stats`.
/// Pure, exact-string tested; MB = `bytes / 1_048_576` (integer).
pub fn format_host_commit_line(c: &HostCommit) -> String {
    format!(
        "host: commit committed_mb={} limit_mb={} free_mb={} pagefile_used_mb={} free_phys_mb={}",
        to_mb(c.committed_bytes),
        to_mb(c.commit_limit_bytes),
        to_mb(c.free_commit_bytes),
        to_mb(c.commit_over_ram_bytes),
        to_mb(c.free_phys_bytes),
    )
}

/// The `/api/v1/status.commit` object — the same figures as the log line, in MB,
/// so the dashboard + the next box measurement read the commit trend without the
/// log. `None` when the read fails / off Windows (a missing key deserializes to
/// `None`, so older clients / the mock stay ok).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HostCommitStatus {
    pub committed_mb: u64,
    pub limit_mb: u64,
    pub free_mb: u64,
    pub commit_over_ram_mb: u64,
    pub free_phys_mb: u64,
}

impl From<HostCommit> for HostCommitStatus {
    fn from(c: HostCommit) -> Self {
        Self {
            committed_mb: to_mb(c.committed_bytes),
            limit_mb: to_mb(c.commit_limit_bytes),
            free_mb: to_mb(c.free_commit_bytes),
            commit_over_ram_mb: to_mb(c.commit_over_ram_bytes),
            free_phys_mb: to_mb(c.free_phys_bytes),
        }
    }
}

/// The `/api/v1/status.commit` value: the live host commit snapshot mapped to MB,
/// or `None` when unreadable / off Windows. Integration seam (reads the OS).
#[cfg_attr(test, mutants::skip)]
pub fn read_status() -> Option<HostCommitStatus> {
    read_host_commit().map(HostCommitStatus::from)
}

/// Read the box-wide commit / pagefile / physical-memory snapshot via
/// `GlobalMemoryStatusEx` + `GetPerformanceInfo`. `None` on any failure.
/// Integration-only.
#[cfg(windows)]
#[cfg_attr(test, mutants::skip)]
fn read_host_commit() -> Option<HostCommit> {
    use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: both structs are correctly sized, zero-initialised POD with their
    // size field (`dwLength` / `cb`) set as the APIs require; each call only
    // writes into its struct and returns 0 on failure.
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) == 0 {
            return None;
        }
        let mut perf: PERFORMANCE_INFORMATION = std::mem::zeroed();
        perf.cb = std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32;
        if GetPerformanceInfo(&mut perf, perf.cb) == 0 {
            return None;
        }
        let committed_bytes = (perf.CommitTotal as u64).saturating_mul(perf.PageSize as u64);
        let total_phys_bytes = status.ullTotalPhys;
        let free_phys_bytes = status.ullAvailPhys;
        // commit_over_ram ≈ committed − (physical in use); no direct field. This
        // is commit NOT backed by resident RAM, not pagefile usage (#207).
        let phys_in_use = total_phys_bytes.saturating_sub(free_phys_bytes);
        let commit_over_ram_bytes = committed_bytes.saturating_sub(phys_in_use);
        Some(HostCommit {
            committed_bytes,
            commit_limit_bytes: status.ullTotalPageFile,
            free_commit_bytes: status.ullAvailPageFile,
            free_phys_bytes,
            total_phys_bytes,
            commit_over_ram_bytes,
        })
    }
}

/// Non-Windows: the box is Windows-only prod; nothing to read here.
#[cfg(not(windows))]
fn read_host_commit() -> Option<HostCommit> {
    None
}

/// Per-minute host-commit logger: on Windows, emit the grep-stable
/// `host: commit …` line once a minute at INFO (target
/// `sp_server::lyrics::host_commit`) beside the pipeline `loop-stats`; off
/// Windows the read is `None` and nothing is logged. Ends on shutdown.
/// Integration glue.
#[cfg_attr(test, mutants::skip)]
pub async fn run_host_commit_logger(mut shutdown: tokio::sync::broadcast::Receiver<()>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Some(c) = read_host_commit() {
                    tracing::info!(target: "sp_server::lyrics::host_commit", "{}", format_host_commit_line(&c));
                }
            }
            _ = shutdown.recv() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HostCommit {
        HostCommit {
            committed_bytes: 8_589_934_592,       // 8192 MiB
            commit_limit_bytes: 66_571_993_088,   // 63488 MiB
            free_commit_bytes: 4_294_967_296,     // 4096 MiB
            free_phys_bytes: 2_147_483_648,       // 2048 MiB
            total_phys_bytes: 17_179_869_184,     // 16384 MiB (not in the line)
            commit_over_ram_bytes: 3_221_225_472, // 3072 MiB
        }
    }

    #[test]
    fn to_mb_is_integer_mebibytes() {
        assert_eq!(to_mb(1_048_576), 1, "one MiB");
        assert_eq!(to_mb(1_048_575), 0, "just under one MiB rounds down");
        assert_eq!(to_mb(8_589_934_592), 8192, "8 GiB = 8192 MiB");
        assert_eq!(to_mb(0), 0);
    }

    #[test]
    fn format_host_commit_line_is_grep_stable() {
        assert_eq!(
            format_host_commit_line(&sample()),
            "host: commit committed_mb=8192 limit_mb=63488 free_mb=4096 commit_over_ram_mb=3072 free_phys_mb=2048"
        );
    }

    #[test]
    fn status_maps_every_field_to_mebibytes() {
        let s = HostCommitStatus::from(sample());
        assert_eq!(
            s,
            HostCommitStatus {
                committed_mb: 8192,
                limit_mb: 63488,
                free_mb: 4096,
                commit_over_ram_mb: 3072,
                free_phys_mb: 2048,
            }
        );
    }

    #[test]
    fn host_commit_status_serde_roundtrips() {
        let s = HostCommitStatus {
            committed_mb: 8192,
            limit_mb: 63488,
            free_mb: 4096,
            commit_over_ram_mb: 3072,
            free_phys_mb: 2048,
        };
        let json = serde_json::to_string(&s).unwrap();
        // #207: the status JSON key is the honest `commit_over_ram_mb`, not the
        // old `pagefile_used_mb`.
        assert!(json.contains("\"commit_over_ram_mb\":3072"), "{json}");
        assert!(!json.contains("pagefile_used_mb"), "{json}");
        let back: HostCommitStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    /// A missing `commit` key (older client / the mock) deserializes to `None`.
    #[test]
    fn absent_commit_deserializes_to_none() {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default)]
            commit: Option<HostCommitStatus>,
        }
        let w: Wrap = serde_json::from_str("{}").unwrap();
        assert!(w.commit.is_none());
    }
}
