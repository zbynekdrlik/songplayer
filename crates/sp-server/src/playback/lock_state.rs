//! Per-pipeline genlock lock-state event window (#149, Lane 1).
//!
//! A small ring of cumulative pacing-counter samples, pushed once per heartbeat
//! (5 s cadence) by the engine as it builds each health snapshot.
//! [`EventWindow::counts_in_window`] reports how many late / repeat / resync
//! events occurred in the last 60 s by differencing the newest cumulative
//! sample from the oldest sample still inside the window; those counts feed
//! `sp_core::genlock::lock_state::derive`.
//!
//! Lives in sp-server (not sp-core) because it is only consumed engine-side and
//! keeps its own history; the derivation *rule* it feeds is the WASM-safe piece
//! in sp-core. Pure and unit-tested — no clock calls; the caller supplies the
//! monotonic timestamp.

use crate::playback::ndi_health::PacingStats;
use sp_core::genlock::UNITS_PER_SECOND;
use sp_core::genlock::lock_state::{LockInputs, LockState, derive};
use sp_core::playback::TransportState;
use std::collections::VecDeque;

/// The lock-state observation window: 60 s in 100-ns units.
pub const LOCK_WINDOW_100NS: i64 = 60 * UNITS_PER_SECOND;

/// Ring capacity. Twelve 5 s intervals span the 60 s window (13 sample points),
/// and time-eviction retains one extra sample just older than the window as a
/// left edge before this hard cap trims back to 13. So the ring holds at most
/// 13 cumulative samples.
pub const EVENT_WINDOW_CAP: usize = 13;

/// One cumulative counter reading at a heartbeat instant. `seq` (boundaries
/// serviced) is the rate base: `counts_in_window` differences it into `slots_w`
/// so `sp_core::genlock::lock_state::derive` can normalise late/repeats against
/// the emitted slots (#168 round 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    ts_100ns: i64,
    seq: u64,
    late: u64,
    repeats: u64,
    resyncs: u64,
}

/// A ring of cumulative pacing-counter samples spanning ~60 s.
#[derive(Clone, Debug, Default)]
pub struct EventWindow {
    ring: VecDeque<Sample>,
}

impl EventWindow {
    /// An empty window.
    pub fn new() -> Self {
        Self {
            ring: VecDeque::new(),
        }
    }

    /// Number of samples currently retained (bounded by [`EVENT_WINDOW_CAP`]).
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether no samples are retained yet.
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Record one cumulative sample at `ts_100ns`, then evict aged-out history.
    ///
    /// A cumulative DECREASE in any counter (including `seq`) can only happen
    /// after the pacer re-anchors (play / seek / new song zeroes its counters),
    /// so it is treated as a reset: the ring is cleared and restarted from this
    /// sample. That stops [`counts_in_window`](Self::counts_in_window) from
    /// differencing across the discontinuity and reporting a bogus giant (or,
    /// with the saturating subtraction, zero) count.
    pub fn push(&mut self, ts_100ns: i64, seq: u64, late: u64, repeats: u64, resyncs: u64) {
        if let Some(back) = self.ring.back() {
            if seq < back.seq
                || late < back.late
                || repeats < back.repeats
                || resyncs < back.resyncs
            {
                self.ring.clear();
            }
        }
        self.ring.push_back(Sample {
            ts_100ns,
            seq,
            late,
            repeats,
            resyncs,
        });
        self.evict(ts_100ns);
    }

    /// Drop samples older than the 60 s window, keeping exactly one sample just
    /// older than the window start as a left edge, then enforce the hard cap.
    /// A sample exactly at the window start (`ts == now − 60 s`) is inclusive
    /// and is retained.
    fn evict(&mut self, now_100ns: i64) {
        let cutoff = now_100ns - LOCK_WINDOW_100NS;
        // Drop the oldest while the SECOND sample is still strictly older than
        // the window start — that keeps the newest strictly-older sample (the
        // +1 left-edge slot) plus everything at/inside the window.
        while self.ring.len() > 1 && self.ring[1].ts_100ns < cutoff {
            self.ring.pop_front();
        }
        // Hard cap (also absorbs the left-edge slot at steady state).
        while self.ring.len() > EVENT_WINDOW_CAP {
            self.ring.pop_front();
        }
    }

    /// `(slots, late, repeats, resyncs)` within the last `window_100ns` (60 s in
    /// production): newest cumulative − the OLDEST sample not older than the
    /// window, `slots` being the differenced `seq` (boundaries serviced = the
    /// rate base for `derive`). When there is less than a full window of history
    /// (cold start), no sample has aged out so the oldest available sample is
    /// the first one — nothing since startup is missed. Empty ring →
    /// `(0, 0, 0, 0)`.
    ///
    /// The subtraction is saturating so a stray reset that slips past
    /// [`push`](Self::push) can never underflow-panic.
    pub fn counts_in_window(&self, now_100ns: i64, window_100ns: i64) -> (u64, u64, u64, u64) {
        let newest = match self.ring.back() {
            None => return (0, 0, 0, 0),
            Some(s) => s,
        };
        let cutoff = now_100ns - window_100ns;
        // Oldest sample with ts >= cutoff (ring is ascending by ts). The newest
        // sample always qualifies (ts == now >= cutoff for window >= 0), so the
        // fallback is only a belt-and-braces guard.
        let baseline = self
            .ring
            .iter()
            .find(|s| s.ts_100ns >= cutoff)
            .unwrap_or(newest);
        (
            newest.seq.saturating_sub(baseline.seq),
            newest.late.saturating_sub(baseline.late),
            newest.repeats.saturating_sub(baseline.repeats),
            newest.resyncs.saturating_sub(baseline.resyncs),
        )
    }
}

/// Push this heartbeat's cumulative pacing counters into the 60 s `window`,
/// difference the window (slots + late/repeats/resyncs), and derive the
/// three-state lock from the clock/pacing/receiver inputs plus the
/// rate-normalised counts (#168 round 6). The single seam `ndi_health.rs` calls
/// so the 999/1000-line file stays line-neutral. `source_fps` is the playing
/// file's nominal fps; `grid_fps` the pacer's fixed grid (`GENLOCK_GRID_FPS`).
/// `transport` is the pipeline's RAW transport (#201, pre-scene-reconciliation):
/// only `Playing` is decoding (#150) — a Paused / Idle output's standby repeats
/// are by design and never read as starvation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lock_for_heartbeat(
    window: &mut EventWindow,
    now_100ns: i64,
    pacing: &PacingStats,
    clock_ok: bool,
    connections: u32,
    source_fps: f32,
    grid_fps: u32,
    transport: TransportState,
) -> (LockState, &'static str) {
    window.push(
        now_100ns,
        pacing.seq,
        pacing.late_frames,
        pacing.repeats,
        pacing.resyncs,
    );
    let (slots_w, late_w, repeats_w, resyncs_w) =
        window.counts_in_window(now_100ns, LOCK_WINDOW_100NS);
    derive(&LockInputs {
        clock_ok,
        pacing_enabled: pacing.enabled,
        connections,
        late_w,
        repeats_w,
        resyncs_w,
        slots_w,
        source_fps,
        grid_fps,
        decoding: transport == TransportState::Playing,
    })
}

#[cfg(test)]
#[path = "lock_state_tests_mutants.rs"]
mod lock_state_tests_mutants;
