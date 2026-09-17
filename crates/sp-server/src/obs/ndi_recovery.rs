//! Receiver-side NDI recovery trigger policy (#127, escalation ladder #173).
//!
//! When SongPlayer sees the state it already names — `degraded_reason`
//! `"no NDI receiver — wall is dark"` — held for N consecutive polls while
//! Playing on program, it nudges OBS over its healthy WebSocket to recover the
//! stranded DistroAV receiver. The remedy ESCALATES through a ladder (#173
//! round 2) because a receiver that is WEDGED inside DistroAV after the sender
//! was recreated several times (repeated SongPlayer restarts) ignores the
//! clear+restore nudge — the box left the on-program wall dark for ~20 min on
//! 17.9.2026 while three clear+restore nudges had no effect:
//!
//! * **Rung 0 — `ClearRestore`**: clear + restore the input's `ndi_source_name`
//!   (the ADVERTISED, case-correct name, round 1), forcing DistroAV to re-run
//!   discovery. Fixes a mis-named / unmatched source.
//! * **Rung 1 — `ToggleSceneItem`**: disable then re-enable the input's scene
//!   item, so DistroAV tears down and recreates the receiver object (what an
//!   operator does by hiding/showing the source).
//! * **Rung 2 — `RecreateInput`**: rename-first recreate (round 3) — rename the
//!   old input away, create the replacement directly under the original name,
//!   prove it exists, restore the scene-item transform/index, THEN remove the
//!   renamed-away old — the strongest receiver-side remedy short of restarting
//!   OBS, safe because a failed create can never empty the scene and no name
//!   freed by a remove is ever reused (dodging DistroAV's async-teardown race).
//!
//! This is ALWAYS **receiver-side** over the OBS WebSocket, NEVER a per-sender
//! `PipelineCommand::RecreateSender` (CLAUDE.md "Disabled subsystems", #60 —
//! structurally unable to fix a receiver-side binding).
//!
//! The decisions are pure functions (`next_step` for the rung, plus the flap
//! bookkeeping) with no I/O, so they are exhaustively unit-testable;
//! `NdiRecoveryTracker` holds the per-pipeline state under interior mutability
//! and is owned by `NdiHealthRegistry` (the `Arc` the playback engine already
//! holds), which is why the trigger needs no new field on `PlaybackEngine`.
//! The I/O that executes each rung lives in `obs/ndi_recovery_io.rs`.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;
use tracing::warn;

/// Consecutive dark-wall polls before the FIRST ladder action. The heartbeat
/// runs every 5 s, so 6 polls ≈ 30 s — long enough that a receiver that simply
/// needs a moment to connect after a scene switch or restart is left alone,
/// short enough that a genuinely stranded receiver is nudged well inside a
/// song.
pub const NUDGE_THRESHOLD_BAD_POLLS: u32 = 6;

/// #173: a "flap" is a receiver that RECOVERED after a nudge and then went dark
/// AGAIN inside this window (100 ns units, = 30 s). The heartbeat runs every 5 s,
/// so a genuine flap (recover then re-dark within a few polls) lands well inside
/// it, while a receiver that stays up for longer than the window and only later
/// goes dark is a fresh outage, not a flap.
pub const FLAP_WINDOW_100NS: i64 = 30 * 10_000_000;

/// #173: consecutive flaps before the recovery tracker ESCALATES — forcing a
/// clear+restore nudge that bypasses the below-threshold skip once (so a
/// normalizing re-apply lands promptly instead of waiting out the dark
/// threshold). Two consecutive flaps escalate: enough to distinguish a
/// genuinely stuck (wrong-case) receiver from a single transient reconnect.
pub const FLAP_ESCALATE_COUNT: u32 = 2;

/// #173 round 2: dark polls that must elapse between ladder rungs. After the
/// first `ClearRestore` at the dark threshold, `ToggleSceneItem` fires 2 further
/// dark polls (~10 s) later, and `RecreateInput` 2 more after that — so a wedged
/// receiver is fully recreated within ~50 s of going dark instead of sitting
/// dark for minutes behind the old 60 s clear+restore cooldown.
///
/// (RED shipped this `u32::MAX` — the TIER-0 "one wrong constant" pattern,
/// `.claude/rules/rust-workspace.md` — so the ladder never advanced past rung 0
/// and the escalation tests failed cleanly; GREEN sets it to `2`.)
pub const LADDER_STEP_SPACING_POLLS: u32 = 2;

/// #173 round 2: dark polls to wait after the ladder completes (`RecreateInput`)
/// before it restarts at `ClearRestore`. 6 polls ≈ 30 s — long enough for a
/// successful recreate to re-attach before another disruptive cycle, but the
/// wall never sits dark for minutes without a fresh escalation.
pub const LADDER_COOLDOWN_POLLS: u32 = 6;

/// #173 round 3: rung 2 (`RecreateInput`) is ENABLED. It was gated off in round 3
/// after the round-2 executor removed `sp-youth_video` and then its `CreateInput`
/// failed, leaving the scene EMPTY (0.54.0-dev.3, box verification 17.9.2026). The
/// executor now RENAMES the old input away (a synchronous rename that frees the
/// original name), CREATES the replacement directly under the original name and
/// PROVES it before removing the renamed-away old
/// (`obs/ndi_recovery_io.rs::recreate_plan`), so a failed create can no longer
/// empty the scene and no name freed by a remove is ever reused — safe to run.
pub const LADDER_RECREATE_ENABLED: bool = true;

/// One rung of the dark-wall recovery ladder (#173 round 2). Ordered by
/// escalating disruption; `ClearRestore` is the round-1 nudge. Serialized onto
/// the `/api/v1/ndi/health` snapshot so the dashboard / E2E can see which rung
/// last fired for a pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum RecoveryStep {
    /// Clear + restore the input's `ndi_source_name` (round-1 nudge).
    ClearRestore,
    /// Toggle the input's scene item off → on.
    ToggleSceneItem,
    /// Rename-first recreate: free the original name by a rename, create the
    /// replacement under it, prove it, then remove the renamed-away old (the
    /// replacement is proven before the old is removed; no removed name is reused).
    RecreateInput,
}

/// Per-pipeline recovery bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NudgeState {
    /// #173: timestamp of the most recent recovery (dark → not-dark) that
    /// followed a nudge in the current outage, pending a flap check. `None`
    /// until a post-nudge recovery is seen, and cleared the moment the pipeline
    /// re-darkens (the flap is counted then).
    pub recovery_at_100ns: Option<i64>,
    /// #173: consecutive recover→re-dark-within-`FLAP_WINDOW_100NS` flaps in the
    /// current outage.
    pub flap_count: u32,
    /// #173: `true` once this outage has escalated via the flap path, so it
    /// escalates at most once.
    pub escalated: bool,
    /// #173: whether ANY nudge has been issued in the current outage — survives a
    /// transient recovery (so a later recovery is still recognised as post-nudge
    /// for flap detection). Cleared only on a stable recovery (state fully
    /// reset).
    pub outage_nudged: bool,
    /// #173 round 2: the NEXT ladder rung to fire in the current dark outage —
    /// `0` = `ClearRestore` (first action), `1` = `ToggleSceneItem`,
    /// `2` = `RecreateInput`, `3` = post-recreate cool-down (then restart at
    /// `ClearRestore`). Reset to `0` on any recovery (the ladder walks one
    /// continuous outage).
    pub ladder_rung: u32,
    /// #173 round 2: the `consecutive_bad_polls` value when the last ladder
    /// action fired, so the rung spacing is measured in dark polls elapsed
    /// since. `None` before the first action of an outage.
    pub last_action_bad_polls: Option<u32>,
}

/// #173 round 2: pure ladder decision — given the tracker state and how many
/// dark polls have elapsed since the last recovery action, which rung (if any)
/// fires now.
///
/// * Rung 0 (`ClearRestore`) fires immediately — the caller gates it on the dark
///   threshold, so "no action yet this outage" means "act now".
/// * Rungs 1 and 2 fire once `LADDER_STEP_SPACING_POLLS` further dark polls have
///   elapsed since the previous rung.
/// * Rung 3 is the post-recreate cool-down; after `LADDER_COOLDOWN_POLLS` dark
///   polls the ladder restarts at `ClearRestore`.
pub fn next_step(state: &NudgeState, dark_polls_since_last_action: u32) -> Option<RecoveryStep> {
    match state.ladder_rung {
        0 => Some(RecoveryStep::ClearRestore),
        1 if dark_polls_since_last_action >= LADDER_STEP_SPACING_POLLS => {
            Some(RecoveryStep::ToggleSceneItem)
        }
        2 if LADDER_RECREATE_ENABLED
            && dark_polls_since_last_action >= LADDER_STEP_SPACING_POLLS =>
        {
            Some(RecoveryStep::RecreateInput)
        }
        // Fallback when rung 2 is gated off (`LADDER_RECREATE_ENABLED = false`):
        // cool down after the toggle, then restart the ladder at `ClearRestore`.
        2 if dark_polls_since_last_action >= LADDER_COOLDOWN_POLLS => {
            Some(RecoveryStep::ClearRestore)
        }
        3 if dark_polls_since_last_action >= LADDER_COOLDOWN_POLLS => {
            Some(RecoveryStep::ClearRestore)
        }
        _ => None,
    }
}

/// Advance the ladder after `step` fired, recording the dark-poll mark so the
/// next rung's spacing is measured from here.
fn record_action(state: &mut NudgeState, step: RecoveryStep, consecutive_bad_polls: u32) {
    state.ladder_rung = match step {
        RecoveryStep::ClearRestore => 1,
        RecoveryStep::ToggleSceneItem => 2,
        RecoveryStep::RecreateInput => 3,
    };
    state.last_action_bad_polls = Some(consecutive_bad_polls);
    state.outage_nudged = true;
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

    /// Evaluate the recovery ladder for one pipeline and apply the resulting
    /// state mutation. Returns `Some(step)` iff the caller should execute that
    /// OBS recovery rung now, else `None`.
    ///
    /// * `is_dark` — true iff the pipeline is Playing on program with the
    ///   dark-wall `degraded_reason` set (`connections == 0`).
    /// * `consecutive_bad_polls` — the snapshot's consecutive bad-poll counter
    ///   (drives the ladder spacing).
    /// * `now_100ns` — the heartbeat's monotonic timestamp (engine `Instant`
    ///   origin, 100 ns units; drives the flap window).
    pub fn evaluate(
        &self,
        playlist_id: i64,
        is_dark: bool,
        consecutive_bad_polls: u32,
        now_100ns: i64,
    ) -> Option<RecoveryStep> {
        let mut guard = match self.states.lock() {
            Ok(g) => g,
            // Poisoned lock → never act (fail safe; the dashboard/log still
            // shows the degraded state).
            Err(_) => return None,
        };
        let state = guard.entry(playlist_id).or_default();

        if !is_dark {
            // Recovered (possibly only transiently). Reset the ladder as the
            // outage is over, but keep flap bookkeeping so a quick re-dark is
            // caught.
            handle_recovery(state, now_100ns);
            return None;
        }

        // Dark: fold this re-dark into the flap counter (if it followed a
        // recent post-nudge recovery).
        note_redark(state, now_100ns);

        // #173: a flapping receiver escalates ONCE — force a clear+restore
        // (case-normalizing) nudge even when the fresh re-dark is below the dark
        // threshold, so a receiver stranded on a wrong-case value is corrected
        // promptly instead of waiting out the threshold while the wall is dark.
        if state.flap_count >= FLAP_ESCALATE_COUNT && !state.escalated {
            state.escalated = true;
            record_action(state, RecoveryStep::ClearRestore, consecutive_bad_polls);
            warn!(
                playlist_id,
                flaps = state.flap_count,
                "ndi-recovery: receiver flapping (recovered then re-dark {}x within {}s) — \
                 escalating to a clear+restore (case normalization)",
                state.flap_count,
                FLAP_WINDOW_100NS / 10_000_000,
            );
            return Some(RecoveryStep::ClearRestore);
        }

        // The ladder's first rung waits for the dark threshold; later rungs are
        // driven by dark polls elapsed since the previous action.
        if consecutive_bad_polls < NUDGE_THRESHOLD_BAD_POLLS {
            return None;
        }
        let since = consecutive_bad_polls.saturating_sub(state.last_action_bad_polls.unwrap_or(0));
        match next_step(state, since) {
            Some(step) => {
                record_action(state, step, consecutive_bad_polls);
                Some(step)
            }
            None => None,
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

/// #173: apply a recovery poll. Resets the ladder (the outage the ladder walks
/// is over) but preserves flap bookkeeping across a transient recovery. Once a
/// recovery has been stable for longer than the flap window, the outage is
/// genuinely over and ALL state (including flap counters) is cleared.
fn handle_recovery(state: &mut NudgeState, now_100ns: i64) {
    state.ladder_rung = 0;
    state.last_action_bad_polls = None;
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

    // ---- pure ladder decision (`next_step`) ------------------------------

    #[test]
    fn next_step_rung0_is_clear_restore_immediately() {
        // No action yet this outage (rung 0) → ClearRestore, regardless of the
        // spacing counter (the caller gates rung 0 on the dark threshold).
        let state = NudgeState::default();
        assert_eq!(next_step(&state, 0), Some(RecoveryStep::ClearRestore));
        assert_eq!(next_step(&state, 99), Some(RecoveryStep::ClearRestore));
    }

    #[test]
    fn next_step_waits_the_spacing_before_toggle_and_recreate() {
        let toggle_pending = NudgeState {
            ladder_rung: 1,
            ..Default::default()
        };
        // Below the spacing → hold.
        assert_eq!(
            next_step(&toggle_pending, LADDER_STEP_SPACING_POLLS - 1),
            None
        );
        // At the spacing → ToggleSceneItem.
        assert_eq!(
            next_step(&toggle_pending, LADDER_STEP_SPACING_POLLS),
            Some(RecoveryStep::ToggleSceneItem)
        );

        let recreate_pending = NudgeState {
            ladder_rung: 2,
            ..Default::default()
        };
        assert_eq!(
            next_step(&recreate_pending, LADDER_STEP_SPACING_POLLS - 1),
            None
        );
        // #173 round 3: rung 2 (RecreateInput) is ENABLED now that the executor
        // creates-first-then-removes (proves the replacement before removing the
        // old input) — the recreate fires at the spacing.
        assert_eq!(
            next_step(&recreate_pending, LADDER_STEP_SPACING_POLLS),
            Some(RecoveryStep::RecreateInput)
        );
    }

    #[test]
    fn next_step_cools_down_then_restarts_at_clear_restore() {
        let cooling = NudgeState {
            ladder_rung: 3,
            ..Default::default()
        };
        assert_eq!(next_step(&cooling, LADDER_COOLDOWN_POLLS - 1), None);
        assert_eq!(
            next_step(&cooling, LADDER_COOLDOWN_POLLS),
            Some(RecoveryStep::ClearRestore)
        );
    }

    // ---- the tracker walks the ladder over a sustained dark outage -------

    #[test]
    fn tracker_escalates_clear_toggle_recreate_over_a_sustained_dark_wall() {
        let tracker = NdiRecoveryTracker::new();
        let pid = 7;
        let mut t = 0i64;

        // Rung 0 at the dark threshold: ClearRestore.
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, t),
            Some(RecoveryStep::ClearRestore)
        );
        // One dark poll later (< spacing) → hold.
        t += 1;
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS + 1, t),
            None
        );
        // Two dark polls after rung 0 → ToggleSceneItem.
        t += 1;
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS + 2, t),
            Some(RecoveryStep::ToggleSceneItem)
        );
        // Hold one poll.
        t += 1;
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS + 3, t),
            None
        );
        // Two dark polls after Toggle → RecreateInput: rung 2 is ENABLED (#173
        // round 3) now that the executor creates-first-then-removes.
        t += 1;
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS + 4, t),
            Some(RecoveryStep::RecreateInput)
        );
    }

    #[test]
    fn tracker_cools_down_after_recreate_then_restarts_the_ladder() {
        let tracker = NdiRecoveryTracker::new();
        let pid = 7;
        let mut t = 0i64;
        // Drive the full ladder: ClearRestore → ToggleSceneItem → RecreateInput.
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, t),
            Some(RecoveryStep::ClearRestore)
        );
        t += 1;
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS + 2, t),
            Some(RecoveryStep::ToggleSceneItem)
        );
        t += 1;
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS + 4, t),
            Some(RecoveryStep::RecreateInput)
        );
        // After the recreate the ladder cools down (rung 3). Below the cool-down
        // (measured from the recreate's dark-poll mark, base+4) → nothing.
        t += 1;
        assert_eq!(
            tracker.evaluate(
                pid,
                true,
                NUDGE_THRESHOLD_BAD_POLLS + 4 + LADDER_COOLDOWN_POLLS - 1,
                t
            ),
            None
        );
        // Past the cool-down → the ladder restarts at ClearRestore.
        t += 1;
        assert_eq!(
            tracker.evaluate(
                pid,
                true,
                NUDGE_THRESHOLD_BAD_POLLS + 4 + LADDER_COOLDOWN_POLLS,
                t
            ),
            Some(RecoveryStep::ClearRestore)
        );
    }

    #[test]
    fn tracker_holds_below_the_dark_threshold() {
        let tracker = NdiRecoveryTracker::new();
        assert_eq!(
            tracker.evaluate(4, true, NUDGE_THRESHOLD_BAD_POLLS - 1, 0),
            None,
            "a receiver that just needs a moment to connect is left alone",
        );
    }

    #[test]
    fn tracker_resets_ladder_on_recovery_then_starts_fresh() {
        let tracker = NdiRecoveryTracker::new();
        let pid = 4;
        // Rung 0 → Toggle.
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, 0),
            Some(RecoveryStep::ClearRestore)
        );
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS + 2, 10),
            Some(RecoveryStep::ToggleSceneItem)
        );
        // Recover, staying up beyond the flap window → outage fully over.
        assert_eq!(tracker.evaluate(pid, false, 0, 20), None);
        assert_eq!(
            tracker.evaluate(pid, false, 0, FLAP_WINDOW_100NS + 30),
            None
        );
        // A fresh dark outage starts at rung 0 (ClearRestore), not mid-ladder.
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, FLAP_WINDOW_100NS + 40),
            Some(RecoveryStep::ClearRestore)
        );
    }

    // ---- flap escalation (round 1, carried into the ladder) --------------

    #[test]
    fn flapping_receiver_escalates_to_a_clear_restore_despite_being_below_threshold() {
        // A receiver that recovers after a nudge and then re-darkens twice
        // within the flap window ESCALATES — a forced ClearRestore even though
        // the fresh re-dark is below the dark threshold (so the ladder would
        // otherwise hold).
        let tracker = NdiRecoveryTracker::new();
        let pid = 4;

        // Outage: dark past threshold → the first ladder rung (ClearRestore).
        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, 0),
            Some(RecoveryStep::ClearRestore)
        );

        // Flap #1: recover, then re-dark (below threshold) inside the window.
        assert_eq!(tracker.evaluate(pid, false, 0, 100), None);
        assert_eq!(tracker.evaluate(pid, true, 1, 200), None);

        // Flap #2: recover, then re-dark again inside the window → ESCALATE to
        // a forced ClearRestore even though consecutive_bad_polls (1) is below
        // the dark threshold.
        assert_eq!(tracker.evaluate(pid, false, 0, 300), None);
        assert_eq!(
            tracker.evaluate(pid, true, 1, 400),
            Some(RecoveryStep::ClearRestore),
            "the 2nd flap must force a normalizing clear+restore below threshold",
        );

        // Escalation fires at most ONCE per outage: a further flap does not
        // re-escalate (still below threshold → hold).
        assert_eq!(tracker.evaluate(pid, false, 0, 500), None);
        assert_eq!(tracker.evaluate(pid, true, 1, 600), None);
    }

    #[test]
    fn a_stable_recovery_resets_flap_state() {
        // A recovery that outlasts the flap window ends the outage, so a later
        // dark starts a fresh flap run (no carried-over escalation).
        let tracker = NdiRecoveryTracker::new();
        let pid = 9;

        assert_eq!(
            tracker.evaluate(pid, true, NUDGE_THRESHOLD_BAD_POLLS, 0),
            Some(RecoveryStep::ClearRestore)
        );
        // Recover and stay recovered beyond the flap window.
        assert_eq!(tracker.evaluate(pid, false, 0, 10), None);
        assert_eq!(
            tracker.evaluate(pid, false, 0, FLAP_WINDOW_100NS + 10),
            None
        );
        // Re-dark long after: NOT a flap (fresh outage), so a single below-
        // threshold dark does not escalate.
        assert_eq!(tracker.evaluate(pid, true, 1, FLAP_WINDOW_100NS + 20), None);
    }
}
