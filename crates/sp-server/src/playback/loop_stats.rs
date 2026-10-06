//! The per-minute `pipeline: loop-stats` line and the shared percentile rule.
//!
//! Pure, cross-platform, Linux-tested.
//!
//! - [`percentile_ceil`] — the `ceil(n·p/100) − 1` index rule shared by the
//!   VBAN send-interval p99, the `SP-program-MAX` timing and the decode bench.
//! - [`LoopStats`] — what the paced heartbeat carries on its `HealthSnapshot`
//!   event for the per-minute `pipeline: loop-stats` line: SongPlayer's own
//!   page faults per minute + working set (#147 round 9, `playback::proc_mem`).
//!   #221 lane 3 deleted the rest with the per-playlist NDI senders: the
//!   `send_video_async` call gauge (#168 round 2) and the SDK-clocked decode
//!   loop's stage maxima (#192 round 3/4, #207).

/// The `p`-th percentile (µs) of `samples` by the `ceil(n·p/100) − 1` index rule
/// (the same rule the emitter's jitter p99 used). Empty → 0. Pure. Any sample
/// sequence: a window's `&VecDeque`, or a run's slice (the decode bench).
pub fn percentile_ceil<'a>(samples: impl IntoIterator<Item = &'a u64>, p: u64) -> u64 {
    let mut v: Vec<u64> = samples.into_iter().copied().collect();
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let n = v.len() as u64;
    // rank = ceil(n·p/100), clamped into [1, n]; index = rank − 1 in [0, n−1].
    let rank = (n * p).div_ceil(100).max(1).min(n);
    v[(rank - 1) as usize]
}

/// The per-minute pipeline telemetry carried on the `HealthSnapshot` event and
/// logged beside `ndi: heartbeat`: SongPlayer's OWN page faults per minute +
/// working set (MiB) over the last full minute (`playback::proc_mem`, #147
/// round 9). `None` (logged `na`) before the first full minute and off
/// Windows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoopStats {
    pub proc_mem: Option<crate::playback::proc_mem::ProcMemGauge>,
}

/// Format the grep-stable `pipeline: loop-stats` line logged beside `ndi:
/// heartbeat` (the `format_genlock_line` precedent). Pure, exact-string tested.
pub fn format_loop_stats_line(ndi_name: &str, s: &LoopStats) -> String {
    use crate::playback::proc_mem::fmt_opt;
    format!(
        "pipeline: loop-stats ndi_name=\"{}\" page_faults_per_min={} working_set_mb={}",
        ndi_name,
        fmt_opt(s.proc_mem.map(|g| g.page_faults_per_min)),
        fmt_opt(s.proc_mem.map(|g| g.working_set_mb)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn percentile_ceil_exact_boundaries() {
        let empty: VecDeque<u64> = VecDeque::new();
        assert_eq!(percentile_ceil(&empty, 99), 0, "empty → 0");
        // n=1 → ceil(1·99/100)=1 → index 0.
        let one: VecDeque<u64> = [7].into_iter().collect();
        assert_eq!(percentile_ceil(&one, 99), 7);
        // n=10, sorted 10..=100 by tens: p99 → ceil(10·99/100)=10 → 10th = 100.
        let ten: VecDeque<u64> = (1..=10).map(|x| x * 10).collect();
        assert_eq!(percentile_ceil(&ten, 99), 100);
        // p50 of 1..=10 → ceil(10·50/100)=5 → 5th = 5.
        let asc: VecDeque<u64> = (1..=10).collect();
        assert_eq!(percentile_ceil(&asc, 50), 5);
        // Unsorted input is sorted first.
        let unsorted: VecDeque<u64> = [5, 1, 9, 3].into_iter().collect();
        assert_eq!(percentile_ceil(&unsorted, 100), 9, "p100 = the max");
    }

    #[test]
    fn format_loop_stats_line_is_grep_stable_before_the_first_full_minute() {
        assert_eq!(
            format_loop_stats_line("SP-fast", &LoopStats::default()),
            "pipeline: loop-stats ndi_name=\"SP-fast\" page_faults_per_min=na working_set_mb=na"
        );
    }

    /// #147 round 9: the paced line carries SongPlayer's own page faults per
    /// minute + working set (MiB) — distinct values so a swapped field diverges.
    #[test]
    fn format_loop_stats_line_carries_page_faults_and_working_set() {
        let ls = LoopStats {
            proc_mem: Some(crate::playback::proc_mem::ProcMemGauge {
                page_faults_per_min: 48_213,
                working_set_mb: 2_300,
            }),
        };
        assert_eq!(
            format_loop_stats_line("SP-slow", &ls),
            "pipeline: loop-stats ndi_name=\"SP-slow\" page_faults_per_min=48213 working_set_mb=2300"
        );
    }
}
