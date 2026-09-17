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

use tracing::warn;

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

/// #173: a "flap" is a receiver that RECOVERED after a nudge and then went dark
/// AGAIN inside this window (100 ns units, = 30 s). The heartbeat runs every 5 s,
/// so a genuine flap (recover then re-dark within a few polls) lands well inside
/// it, while a receiver that stays up for longer than the window and only later
/// goes dark is a fresh outage, not a flap.
pub const FLAP_WINDOW_100NS: i64 = 30 * 10_000_000;

/// #173: consecutive flaps before the recovery tracker ESCALATES — forcing a
/// nudge that bypasses the below-threshold / cooldown skip once (so a
/// normalizing re-apply lands promptly instead of after the full 60 s cooldown).
///
/// GREEN sets this to `2`. The RED commit ships a value the flap counter never
/// reaches so escalation never fires and the escalation unit test fails cleanly
/// (TIER-0 "one wrong constant" RED pattern, `.claude/rules/rust-workspace.md`).
pub const FLAP_ESCALATE_COUNT: u32 = 1_000_000;

/// Per-pipeline recovery bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NudgeState {
    /// Nudges issued in the current (unbroken) dark-wall outage.
    pub attempts: u32,
    /// Monotonic timestamp (100 ns units, engine `Instant` origin) of the last
    /// nudge, or `None` if none has been issued this outage.
    pub last_nudge_100ns: Option<i64>,
    /// #173: timestamp of the most recent recovery (dark → not-dark) that
    /// followed a nudge in the current outage, pending a flap check. `None`
    /// until a post-nudge recovery is seen, and cleared the moment the pipeline
    /// re-darkens (the flap is counted then).
    pub recovery_at_100ns: Option<i64>,
    /// #173: consecutive recover→re-dark-within-`FLAP_WINDOW_100NS` flaps in the
    /// current outage.
    pub flap_count: u32,
    /// #173: `true` once this outage has escalated, so it escalates at most once.
    pub escalated: bool,
    /// #173: whether ANY nudge has been issued in the current outage — survives a
    /// transient recovery (which resets `attempts`) so a later recovery is still
    /// recognised as post-nudge for flap detection. Cleared only on a stable
    /// recovery (state fully reset).
    pub outage_nudged: bool,
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
    state: &NudgeState,
    is_dark: bool,
    consecutive_bad_polls: u32,
    now_100ns: i64,
) -> NudgeDecision {
    // Not dark → the outage (if any) is over; reset so the next one gets a
    // fresh attempt budget.
    if !is_dark {
        return NudgeDecision::Reset;
    }
    // Dark, but give a normally-reconnecting receiver time before the first
    // nudge.
    if consecutive_bad_polls < NUDGE_THRESHOLD_BAD_POLLS {
        return NudgeDecision::Skip(SkipReason::BelowThreshold);
    }
    // Don't nudge forever — a genuinely misconfigured/absent OBS input should
    // fall back to log-only after a bounded number of attempts.
    if state.attempts >= NUDGE_MAX_ATTEMPTS {
        return NudgeDecision::Skip(SkipReason::MaxAttempts);
    }
    // Space nudges out so DistroAV's re-discovery from the previous nudge can
    // land before we try again.
    if let Some(last) = state.last_nudge_100ns
        && now_100ns.saturating_sub(last) < NUDGE_COOLDOWN_100NS
    {
        return NudgeDecision::Skip(SkipReason::Cooldown);
    }
    NudgeDecision::Nudge
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

        if !is_dark {
            // Recovered (possibly only transiently). Reset the nudge budget as
            // before, but keep flap bookkeeping so a quick re-dark is caught.
            handle_recovery(state, now_100ns);
            return false;
        }

        // Dark: fold this re-dark into the flap counter (if it followed a
        // recent post-nudge recovery).
        note_redark(state, now_100ns);

        // #173: a flapping receiver escalates ONCE — force a nudge even when the
        // normal cadence would skip (below threshold / cooldown). With #173's
        // normalizing reapply, that forced nudge re-applies the ADVERTISED name,
        // so a receiver stranded on a wrong-case value gets corrected promptly
        // instead of waiting the full cooldown while the wall stays dark.
        if state.flap_count >= FLAP_ESCALATE_COUNT && !state.escalated {
            state.escalated = true;
            state.attempts += 1;
            state.last_nudge_100ns = Some(now_100ns);
            state.outage_nudged = true;
            warn!(
                playlist_id,
                flaps = state.flap_count,
                "ndi-recovery: receiver flapping (recovered then re-dark {}x within {}s) — \
                 escalating to case normalization",
                state.flap_count,
                FLAP_WINDOW_100NS / 10_000_000,
            );
            return true;
        }

        match decide(state, true, consecutive_bad_polls, now_100ns) {
            NudgeDecision::Nudge => {
                state.attempts += 1;
                state.last_nudge_100ns = Some(now_100ns);
                state.outage_nudged = true;
                true
            }
            // Reset is unreachable while is_dark; Skip → no nudge this poll.
            _ => false,
        }
    }
}

/// #173: fold a re-dark into the flap counter. If the pipeline recently
/// recovered after a nudge (`recovery_at_100ns` set) and re-darkened within the
/// flap window, count a flap; if the recovery outlasted the window, it was a
/// fresh outage, not a flap, so the flap run resets.
fn note_redark(state: &mut NudgeState, now_100ns: i64) {
    if let Some(rec) = state.recovery_at_100ns.take() {
        if now_100ns.saturating_sub(rec) < FLAP_WINDOW_100NS {
            state.flap_count += 1;
        } else {
            state.flap_count = 0;
            state.escalated = false;
        }
    }
}

/// #173: apply a recovery poll. Resets the per-outage nudge budget (the prior
/// behaviour — a recovered pipeline gets a fresh nudge budget on its next
/// outage), but preserves flap bookkeeping across a transient recovery. Once a
/// recovery has been stable for longer than the flap window, the outage is
/// genuinely over and ALL state (including flap counters) is cleared.
fn handle_recovery(state: &mut NudgeState, now_100ns: i64) {
    state.attempts = 0;
    state.last_nudge_100ns = None;
    if state.outage_nudged && state.recovery_at_100ns.is_none() {
        state.recovery_at_100ns = Some(now_100ns);
    }
    if let Some(rec) = state.recovery_at_100ns {
        if now_100ns.saturating_sub(rec) >= FLAP_WINDOW_100NS {
            *state = NudgeState::default();
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

    #[test]
    fn flapping_receiver_escalates_to_a_nudge_despite_a_skip() {
        // #173: a receiver that recovers after a nudge and then re-darkens
        // twice within the flap window must ESCALATE — a forced nudge even
        // though the fresh re-dark is below the nudge threshold (so the normal
        // cadence would Skip). With #173's normalizing reapply, that forced
        // nudge re-applies the ADVERTISED name.
        let tracker = NdiRecoveryTracker::new();
        let pid = 4;

        // Outage: dark past threshold → the first (normal) nudge.
        assert!(tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, 0));

        // Flap #1: recover, then re-dark (below threshold) inside the window.
        assert!(!tracker.evaluate(pid, false, 0, 100));
        assert!(!tracker.evaluate(pid, true, 1, 200));

        // Flap #2: recover, then re-dark again inside the window → ESCALATE,
        // even though consecutive_bad_polls (1) is below the nudge threshold.
        assert!(!tracker.evaluate(pid, false, 0, 300));
        assert!(
            tracker.evaluate(pid, true, 1, 400),
            "the 2nd flap must force a normalizing nudge despite being below threshold",
        );

        // Escalation fires at most ONCE per outage: a further flap does not
        // re-escalate (still below threshold → skip).
        assert!(!tracker.evaluate(pid, false, 0, 500));
        assert!(!tracker.evaluate(pid, true, 1, 600));
    }

    #[test]
    fn a_stable_recovery_resets_flap_state() {
        // #173: a recovery that outlasts the flap window ends the outage, so a
        // later dark starts a fresh flap run (no carried-over escalation).
        let tracker = NdiRecoveryTracker::new();
        let pid = 9;

        assert!(tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, 0));
        // Recover and stay recovered beyond the flap window.
        assert!(!tracker.evaluate(pid, false, 0, 10));
        assert!(!tracker.evaluate(pid, false, 0, FLAP_WINDOW_100NS + 10));
        // Re-dark long after: NOT a flap (fresh outage), so a single below-
        // threshold dark does not escalate.
        assert!(!tracker.evaluate(pid, true, 1, FLAP_WINDOW_100NS + 20));
    }
}
