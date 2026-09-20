//! Pipeline-loop stage timing + submit-call histogram (#192 round 3).
//!
//! Pure, cross-platform, Linux-tested. Two gauges attribute a producer stall to
//! its stage so the next round names the blocker (decode vs submit vs audio):
//!
//! - [`SubmitHist`] — a bounded ring of per-call `send_video_async` durations
//!   (µs) the `FrameSubmitter` feeds; [`drain`](SubmitHist::drain) returns
//!   `(max, p99)` and resets, surfaced through `WindowStats` into the per-minute
//!   `ndi: heartbeat` line.
//! - [`LoopStageMax`] — the max decode / submit / audio µs of one decode-loop
//!   iteration over a window; [`drain`](LoopStageMax::drain) returns the
//!   [`LoopStageStats`] and resets.
//!
//! Both drain on the same cadence as `FrameSubmitter::drain_window` (per 5 s
//! heartbeat), so the per-minute `ndi: heartbeat` carries the last window's
//! worst stage. Assembled into [`LoopStats`] on the `HealthSnapshot` event and
//! logged beside `ndi: heartbeat` via [`format_loop_stats_line`] (the
//! `format_genlock_line` precedent). The pure structs are mutation-scored; only
//! the trivial [`timed`] clock wrapper is `mutants::skip`.

use std::collections::VecDeque;

/// Safety bound on the submit-call sample deque. `drain()` clears it on every
/// heartbeat (≈ 5 s ≈ 150 frames), so the p99 is per heartbeat window; the bound
/// only matters if a heartbeat is ever skipped.
const SUBMIT_WINDOW: usize = 900;

/// Run `f`, returning its result and the wall µs it took. A thin timing wrapper
/// so the decode loop stays ONE line per stage (`pipeline.rs` is at the
/// 1000-line cap). `mutants::skip` — the real-clock reading is not assertable.
#[cfg_attr(test, mutants::skip)]
pub fn timed<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let t = std::time::Instant::now();
    let r = f();
    (r, t.elapsed().as_micros() as u64)
}

/// The `p`-th percentile (µs) of `samples` by the `ceil(n·p/100) − 1` index rule
/// (the same rule the emitter's jitter p99 uses). Empty → 0. Pure.
pub fn percentile_ceil(samples: &VecDeque<u64>, p: u64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut v: Vec<u64> = samples.iter().copied().collect();
    v.sort_unstable();
    let n = v.len() as u64;
    // rank = ceil(n·p/100), clamped into [1, n]; index = rank − 1 in [0, n−1].
    let rank = (n * p).div_ceil(100).max(1).min(n);
    v[(rank - 1) as usize]
}

/// Bounded ring of per-call `send_video_async` durations (µs). Never grows past
/// [`SUBMIT_WINDOW`]; [`drain`](Self::drain) reads `(max, p99)` and clears it.
#[derive(Debug, Default)]
pub struct SubmitHist {
    samples: VecDeque<u64>,
}

impl SubmitHist {
    /// Record one `send_video_async` call duration (µs).
    pub fn observe(&mut self, us: u64) {
        self.samples.push_back(us);
        // ONE push can overshoot the window by at most one, so a single
        // conditional pop keeps the bound — never a `while` (a mutated
        // comparison would spin forever on an empty deque; mutation timeout).
        if self.samples.len() > SUBMIT_WINDOW {
            self.samples.pop_front();
        }
    }

    /// Read the window's `(max, p99)` µs and CLEAR it (per-heartbeat drain).
    pub fn drain(&mut self) -> (u64, u64) {
        let max = self.samples.iter().copied().max().unwrap_or(0);
        let p99 = percentile_ceil(&self.samples, 99);
        self.samples.clear();
        (max, p99)
    }
}

/// The worst single-iteration decode / submit / audio cost (µs) over a window.
/// Fed once per decode-loop iteration; [`drain`](Self::drain) returns the stats
/// and resets.
#[derive(Debug, Default)]
pub struct LoopStageMax {
    decode_us: u64,
    submit_us: u64,
    audio_us: u64,
    catchup_dropped: u64,
}

impl LoopStageMax {
    /// Fold one loop iteration's three stage durations (µs) into the maxima.
    pub fn observe(&mut self, decode_us: u64, submit_us: u64, audio_us: u64) {
        self.decode_us = self.decode_us.max(decode_us);
        self.submit_us = self.submit_us.max(submit_us);
        self.audio_us = self.audio_us.max(audio_us);
    }

    /// Fold a #192-round-4 DROPPED iteration (video not submitted): the decode +
    /// audio maxima with a zero submit cost, and bump the catch-up drop COUNT
    /// (a sum over the window, not a max). Used only on the drop path.
    pub fn observe_drop(&mut self, decode_us: u64, audio_us: u64) {
        self.observe(decode_us, 0, audio_us);
        self.catchup_dropped = self.catchup_dropped.saturating_add(1);
    }

    /// Read the window's stage maxima and RESET (per-heartbeat drain).
    pub fn drain(&mut self) -> LoopStageStats {
        let out = LoopStageStats {
            decode_us_max: self.decode_us,
            submit_us_max: self.submit_us,
            audio_us_max: self.audio_us,
            catchup_dropped: self.catchup_dropped,
        };
        *self = Self::default();
        out
    }
}

/// The decode-loop stage maxima for one heartbeat window (µs). Pure data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoopStageStats {
    pub decode_us_max: u64,
    pub submit_us_max: u64,
    pub audio_us_max: u64,
    /// #192 round 4: video frames dropped by the catch-up in this window (a
    /// count, not µs) — the direct producer-stall meter.
    pub catchup_dropped: u64,
}

/// The #192-round-3 per-minute pipeline telemetry carried on the
/// `HealthSnapshot` event and logged beside `ndi: heartbeat`: the raw
/// `send_video_async` call max/p99 (from the submitter's [`SubmitHist`] via
/// `WindowStats`) plus the decode loop's stage maxima (from [`LoopStageMax`]).
/// All µs. `Default` (all-zero) is what an idle / paced / paused heartbeat
/// carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoopStats {
    pub submit_call_us_max: u64,
    pub submit_call_us_p99: u64,
    pub decode_us_max: u64,
    pub submit_us_max: u64,
    pub audio_us_max: u64,
    /// #192 round 4: catch-up video-frame drops this window (a count, not µs).
    pub catchup_dropped: u64,
}

impl LoopStats {
    /// Assemble the wire struct from the submitter's `(max, p99)` submit-call
    /// gauge and the decode loop's [`LoopStageStats`]. One helper so the
    /// heartbeat call site stays a single line.
    pub fn from_parts(
        submit_call_us_max: u64,
        submit_call_us_p99: u64,
        stage: LoopStageStats,
    ) -> Self {
        Self {
            submit_call_us_max,
            submit_call_us_p99,
            decode_us_max: stage.decode_us_max,
            submit_us_max: stage.submit_us_max,
            audio_us_max: stage.audio_us_max,
            catchup_dropped: stage.catchup_dropped,
        }
    }
}

/// Format the grep-stable `pipeline: loop-stats` line logged beside `ndi:
/// heartbeat` (the `format_genlock_line` precedent). Pure, exact-string tested.
pub fn format_loop_stats_line(ndi_name: &str, s: &LoopStats) -> String {
    format!(
        "pipeline: loop-stats ndi_name=\"{}\" submit_call_us_max={} submit_call_us_p99={} decode_us_max={} submit_us_max={} audio_us_max={} catchup_dropped={}",
        ndi_name,
        s.submit_call_us_max,
        s.submit_call_us_p99,
        s.decode_us_max,
        s.submit_us_max,
        s.audio_us_max,
        s.catchup_dropped,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(vals: &[u64]) -> SubmitHist {
        let mut h = SubmitHist::default();
        for &v in vals {
            h.observe(v);
        }
        h
    }

    #[test]
    fn submit_hist_drain_reports_max_and_p99_then_clears() {
        // 100 samples 1..=100: max 100, p99 = ceil(100·99/100)=99th → value 99.
        let mut h = hist(&(1..=100).collect::<Vec<_>>());
        let (max, p99) = h.drain();
        assert_eq!(max, 100, "window max");
        assert_eq!(p99, 99, "ceil(n·0.99) index = the 99th of 100 = value 99");
        // Drained → cleared → the next drain is all-zero.
        assert_eq!(h.drain(), (0, 0), "an empty window drains to (0, 0)");
    }

    #[test]
    fn submit_hist_is_bounded_and_keeps_the_recent_window() {
        // Push more than the window; the oldest (smallest) samples are dropped,
        // so the max reflects only the retained recent SUBMIT_WINDOW samples.
        let mut h = SubmitHist::default();
        for v in 0..(SUBMIT_WINDOW as u64 + 50) {
            h.observe(v);
        }
        let (max, _p99) = h.drain();
        assert_eq!(max, SUBMIT_WINDOW as u64 + 49, "newest sample retained");
        // The 50 oldest (0..=49) were evicted, so a single old value can't be max.
    }

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
    fn loop_stage_max_keeps_the_worst_of_each_stage_then_resets() {
        let mut s = LoopStageMax::default();
        s.observe(10, 5, 2);
        s.observe(3, 40, 1); // submit spikes
        s.observe(7, 8, 30); // audio spikes
        let drained = s.drain();
        assert_eq!(
            drained,
            LoopStageStats {
                decode_us_max: 10,
                submit_us_max: 40,
                audio_us_max: 30,
                catchup_dropped: 0, // no drops this window
            }
        );
        // Reset on drain: a fresh observe starts a new window.
        s.observe(1, 1, 1);
        assert_eq!(
            s.drain(),
            LoopStageStats {
                decode_us_max: 1,
                submit_us_max: 1,
                audio_us_max: 1,
                catchup_dropped: 0,
            }
        );
    }

    #[test]
    fn observe_drop_counts_drops_with_zero_submit_and_folds_decode_audio() {
        // A #192-round-4 dropped iteration: decode + audio maxima fold, the submit
        // stage stays 0 (nothing submitted), and the drop COUNT accumulates.
        let mut s = LoopStageMax::default();
        s.observe_drop(12, 4);
        s.observe_drop(9, 30); // audio spikes on a dropped frame too
        s.observe_drop(20, 1); // decode spikes
        let drained = s.drain();
        assert_eq!(
            drained,
            LoopStageStats {
                decode_us_max: 20,
                submit_us_max: 0, // never submitted on a drop → stays 0
                audio_us_max: 30,
                catchup_dropped: 3, // three drops summed
            }
        );
        // Reset on drain: the drop count starts over next window.
        assert_eq!(s.drain().catchup_dropped, 0);
    }

    #[test]
    fn observe_drop_and_observe_share_one_window() {
        // Submitted frames keep their submit_us max; dropped frames add to the
        // count without disturbing it.
        let mut s = LoopStageMax::default();
        s.observe(5, 22, 3); // a submitted frame
        s.observe_drop(8, 6); // a dropped frame
        let drained = s.drain();
        assert_eq!(
            drained.submit_us_max, 22,
            "the submitted frame's cost survives"
        );
        assert_eq!(drained.decode_us_max, 8);
        assert_eq!(drained.catchup_dropped, 1);
    }

    #[test]
    fn loop_stats_from_parts_bundles_submit_call_and_stage() {
        let stage = LoopStageStats {
            decode_us_max: 11,
            submit_us_max: 22,
            audio_us_max: 33,
            catchup_dropped: 7,
        };
        let ls = LoopStats::from_parts(954_000, 88_000, stage);
        assert_eq!(
            ls,
            LoopStats {
                submit_call_us_max: 954_000,
                submit_call_us_p99: 88_000,
                decode_us_max: 11,
                submit_us_max: 22,
                audio_us_max: 33,
                catchup_dropped: 7, // carried through from the stage
            }
        );
    }

    #[test]
    fn format_loop_stats_line_is_grep_stable() {
        let ls = LoopStats {
            submit_call_us_max: 954000,
            submit_call_us_p99: 88000,
            decode_us_max: 11,
            submit_us_max: 22,
            audio_us_max: 33,
            catchup_dropped: 5,
        };
        assert_eq!(
            format_loop_stats_line("SP-fast", &ls),
            "pipeline: loop-stats ndi_name=\"SP-fast\" submit_call_us_max=954000 submit_call_us_p99=88000 decode_us_max=11 submit_us_max=22 audio_us_max=33 catchup_dropped=5"
        );
    }
}
