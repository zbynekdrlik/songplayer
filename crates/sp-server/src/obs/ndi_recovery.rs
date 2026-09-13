//! Receiver-side NDI recovery trigger policy (#127).
//!
//! When SongPlayer sees the state it already names — `degraded_reason`
//! `"no NDI receiver — wall is dark"` — held for N consecutive polls while
//! Playing on program, it nudges OBS over its healthy WebSocket to clear +
//! restore the matching NDI input's `ndi_source_name`, forcing DistroAV to
//! re-run discovery (the owner's proven manual fix, issue #127).
//!
//! This is a **receiver-side** nudge over the OBS WebSocket, NEVER a
//! per-sender `PipelineCommand::RecreateSender` (CLAUDE.md "Disabled
//! subsystems", #60 — structurally unable to fix a receiver-side binding).
//!
//! The decision is a pure function (`decide`) with no I/O and no clock, so it
//! is exhaustively unit-testable; `NdiRecoveryTracker` holds the per-pipeline
//! state under interior mutability and is owned by `NdiHealthRegistry` (the
//! `Arc` the playback engine already holds), which is why the trigger needs no
//! new field on `PlaybackEngine`.

use std::collections::HashMap;
use std::sync::Mutex;

/// Consecutive dark-wall polls before the FIRST nudge. The heartbeat runs
/// every 5 s, so 6 polls ≈ 30 s — long enough that a receiver that simply
/// needs a moment to connect after a scene switch or restart is left alone,
/// short enough that a genuinely stranded receiver is nudged well inside a
/// song.
pub const NUDGE_THRESHOLD_BAD_POLLS: u32 = 6;

/// Minimum spacing between nudges for one pipeline, in 100 ns units (= 60 s).
/// A nudge forces DistroAV re-discovery which takes a few seconds; the
/// cooldown lets that land before another attempt so we do not hammer OBS on
/// every 5 s heartbeat.
pub const NUDGE_COOLDOWN_100NS: i64 = 60 * 10_000_000;

/// Maximum nudges per outage before giving up (and continuing to log only).
/// Reset to zero the moment the pipeline recovers, so a later outage gets a
/// fresh set of attempts.
pub const NUDGE_MAX_ATTEMPTS: u32 = 3;

/// Per-pipeline recovery bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NudgeState {
    /// Nudges issued in the current (unbroken) dark-wall outage.
    pub attempts: u32,
    /// Monotonic timestamp (100 ns units, engine `Instant` origin) of the last
    /// nudge, or `None` if none has been issued this outage.
    pub last_nudge_100ns: Option<i64>,
}

/// Why a dark pipeline was not nudged on this poll (logging only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// Dark, but not yet past the consecutive-poll threshold.
    BelowThreshold,
    /// Nudged too recently (within the cooldown window).
    Cooldown,
    /// Exhausted the per-outage attempt cap.
    MaxAttempts,
}

/// Outcome of the pure trigger policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NudgeDecision {
    /// Not dark → the caller resets the pipeline's state (fresh attempts on the
    /// next outage).
    Reset,
    /// Dark, but do not nudge now.
    Skip(SkipReason),
    /// Nudge now → the caller sends the OBS command, bumps `attempts`, and
    /// records `last_nudge_100ns`.
    Nudge,
}

/// Pure trigger policy. Everything is an argument (no I/O, no clock), so this
/// is fully unit-testable.
///
/// * `is_dark` — true iff the pipeline is Playing on program with the dark-wall
///   `degraded_reason` set (`connections == 0`).
/// * `consecutive_bad_polls` — the snapshot's consecutive bad-poll counter.
/// * `now_100ns` — the heartbeat's monotonic timestamp (engine `Instant`
///   origin, 100 ns units).
pub fn decide(
    _state: &NudgeState,
    _is_dark: bool,
    _consecutive_bad_polls: u32,
    _now_100ns: i64,
) -> NudgeDecision {
    // RED stub — always skips. The real policy lands in the GREEN fix.
    NudgeDecision::Skip(SkipReason::BelowThreshold)
}

/// Shared, interior-mutability tracker holding one `NudgeState` per pipeline.
/// Owned by `NdiHealthRegistry` so the engine reaches it through the `Arc` it
/// already holds — no new `PlaybackEngine` field. The single writer is the
/// engine's health-snapshot handler, so lock contention is nil.
#[derive(Default)]
pub struct NdiRecoveryTracker {
    states: Mutex<HashMap<i64, NudgeState>>,
}

impl NdiRecoveryTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate the policy for one pipeline and apply the resulting state
    /// mutation. Returns `true` iff the caller should issue an OBS nudge now.
    pub fn evaluate(
        &self,
        playlist_id: i64,
        is_dark: bool,
        consecutive_bad_polls: u32,
        now_100ns: i64,
    ) -> bool {
        let mut guard = match self.states.lock() {
            Ok(g) => g,
            // Poisoned lock → never nudge (fail safe; the dashboard/log still
            // shows the degraded state).
            Err(_) => return false,
        };
        let state = guard.entry(playlist_id).or_default();
        match decide(state, is_dark, consecutive_bad_polls, now_100ns) {
            NudgeDecision::Reset => {
                *state = NudgeState::default();
                false
            }
            NudgeDecision::Skip(_) => false,
            NudgeDecision::Nudge => {
                state.attempts += 1;
                state.last_nudge_100ns = Some(now_100ns);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_dark_returns_reset() {
        let state = NudgeState {
            attempts: 2,
            last_nudge_100ns: Some(500),
        };
        assert_eq!(decide(&state, false, 0, 1_000), NudgeDecision::Reset);
    }

    #[test]
    fn dark_below_threshold_skips() {
        let state = NudgeState::default();
        assert_eq!(
            decide(&state, true, NUDGE_THRESHOLD_BAD_POLLS - 1, 0),
            NudgeDecision::Skip(SkipReason::BelowThreshold),
        );
    }

    #[test]
    fn dark_at_threshold_first_time_nudges() {
        let state = NudgeState::default();
        assert_eq!(
            decide(&state, true, NUDGE_THRESHOLD_BAD_POLLS, 0),
            NudgeDecision::Nudge,
        );
    }

    #[test]
    fn dark_within_cooldown_skips() {
        let state = NudgeState {
            attempts: 1,
            last_nudge_100ns: Some(0),
        };
        // now is inside the cooldown window → skip.
        assert_eq!(
            decide(
                &state,
                true,
                NUDGE_THRESHOLD_BAD_POLLS,
                NUDGE_COOLDOWN_100NS - 1
            ),
            NudgeDecision::Skip(SkipReason::Cooldown),
        );
    }

    #[test]
    fn dark_after_cooldown_nudges_again() {
        let state = NudgeState {
            attempts: 1,
            last_nudge_100ns: Some(0),
        };
        assert_eq!(
            decide(
                &state,
                true,
                NUDGE_THRESHOLD_BAD_POLLS,
                NUDGE_COOLDOWN_100NS
            ),
            NudgeDecision::Nudge,
        );
    }

    #[test]
    fn max_attempts_exhausted_skips() {
        let state = NudgeState {
            attempts: NUDGE_MAX_ATTEMPTS,
            last_nudge_100ns: Some(0),
        };
        // Well past cooldown, but the attempt cap is exhausted.
        assert_eq!(
            decide(&state, true, 999, NUDGE_COOLDOWN_100NS * 100),
            NudgeDecision::Skip(SkipReason::MaxAttempts),
        );
    }

    #[test]
    fn tracker_nudges_once_then_cooldowns_then_recovers() {
        let tracker = NdiRecoveryTracker::new();
        let pid = 4;

        // First dark poll past threshold → nudge.
        assert!(tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, 0));
        // Immediately again (still dark, still past threshold) → cooldown skip.
        assert!(!tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS + 1, 1_000));
        // Past cooldown → second nudge.
        assert!(tracker.evaluate(
            pid,
            true,
            NUDGE_THRESHOLD_BAD_POLLS + 2,
            NUDGE_COOLDOWN_100NS
        ));

        // Recovery resets attempts so a later outage gets a fresh nudge
        // immediately at threshold.
        assert!(!tracker.evaluate(pid, false, 0, NUDGE_COOLDOWN_100NS + 10));
        assert!(tracker.evaluate(
            pid,
            true,
            NUDGE_THRESHOLD_BAD_POLLS,
            NUDGE_COOLDOWN_100NS + 20
        ));
    }

    #[test]
    fn tracker_stops_after_max_attempts_until_recovery() {
        let tracker = NdiRecoveryTracker::new();
        let pid = 7;
        let mut now = 0i64;
        // Three nudges (each past the cooldown), then the cap holds.
        for _ in 0..NUDGE_MAX_ATTEMPTS {
            assert!(tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, now));
            now += NUDGE_COOLDOWN_100NS;
        }
        // Fourth attempt, still dark, well past cooldown → capped.
        assert!(!tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, now));
    }
}
