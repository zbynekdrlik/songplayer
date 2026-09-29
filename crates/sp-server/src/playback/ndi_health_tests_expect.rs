//! #221 L4a: the dark-wall reason, the #173 ladder and the #196 post-restart
//! self-check expect a receiver only where cg OBS was told to show the
//! playlist (`ndi_health_expect::receiver_expected`, fed from `legacy_cg`);
//! the state label stays keyed on on-air. Shares the rig of
//! `ndi_health_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "ndi_health_tests_expect.rs"] mod tests_expect;`.

use std::time::{Duration, Instant};

use sp_core::health::{NO_RECEIVER_AFTER_RESTART_REASON, SELF_CHECK_DELAY};

use super::tests::{dark_wall_event, fresh_engine_with_obs_cmd};
use super::{DARK_WALL_REASON, PlaybackStateLabel};
use crate::obs::ObsCommand;
use crate::obs::ndi_recovery::{NUDGE_THRESHOLD_BAD_POLLS, RecoveryStep};
use crate::playback::state::PlayState;

/// A playlist on air (playing, its scene on program) whose output cg OBS
/// was NOT told to show: 0 receivers is normal there. No degraded reason, no
/// ladder rung, no OBS command — while the label still reads Playing (the
/// badge and the idle gates are on-air). Once cg OBS is told to show it, the
/// same poll is the dark wall and its ladder, as before L4a.
#[tokio::test]
async fn only_the_output_cg_obs_was_told_to_show_can_be_a_dark_wall() {
    for cg_shown in [Some(7), None] {
        let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
        engine.ensure_pipeline(4, "SP-slow");
        engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
        engine.set_scene_active_for_test(4, true);
        engine.set_cg_shown_for_test(cg_shown);
        let now = Instant::now();
        for polls in [2, NUDGE_THRESHOLD_BAD_POLLS, NUDGE_THRESHOLD_BAD_POLLS + 4] {
            engine.handle_health_snapshot(4, dark_wall_event(now, polls));
        }
        let snap = registry.snapshots()[0].clone();
        assert_eq!(snap.state, PlaybackStateLabel::Playing, "{cg_shown:?}");
        assert_eq!(snap.degraded_reason, None, "{cg_shown:?}");
        assert_eq!(snap.recovery_step, None, "{cg_shown:?}");
        assert!(
            obs_rx.try_recv().is_err(),
            "no rung ran against cg OBS's inputs ({cg_shown:?})"
        );

        // cg OBS is told to show it: now a receiver is expected.
        engine.set_cg_shown_for_test(Some(4));
        engine.handle_health_snapshot(4, dark_wall_event(now, NUDGE_THRESHOLD_BAD_POLLS));
        let snap = registry.snapshots()[0].clone();
        assert_eq!(snap.degraded_reason.as_deref(), Some(DARK_WALL_REASON));
        assert_eq!(snap.recovery_step, Some(RecoveryStep::ClearRestore));
        match obs_rx.try_recv() {
            Ok(ObsCommand::NudgeNdiReceiver { ndi_name, step }) => {
                assert_eq!(ndi_name, "SP-slow");
                assert_eq!(step, RecoveryStep::ClearRestore);
            }
            Ok(other) => panic!("expected NudgeNdiReceiver, got {other:?}"),
            Err(e) => panic!("expected a NudgeNdiReceiver, got none: {e:?}"),
        }
    }
}

/// #196's post-restart self-check flags an output that is expected to have a
/// receiver: 30 s after the senders were ready, an on-air output cg OBS does
/// not show (and that had no receiver before the restart) is not flagged;
/// the one cg OBS shows is.
#[tokio::test]
async fn the_post_restart_self_check_expects_a_receiver_only_where_cg_obs_was_told() {
    let (mut engine, registry, _obs_rx) = fresh_engine_with_obs_cmd().await;
    let ready = Instant::now().checked_sub(SELF_CHECK_DELAY + Duration::from_secs(1));
    *registry.senders_ready_at.write().unwrap() = ready;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(4, true);

    engine.set_cg_shown_for_test(Some(7));
    // One bad poll: below the dark-wall gate, so only the self-check speaks.
    engine.handle_health_snapshot(4, dark_wall_event(Instant::now(), 1));
    assert_eq!(registry.snapshots()[0].degraded_reason, None);

    engine.set_cg_shown_for_test(Some(4));
    engine.handle_health_snapshot(4, dark_wall_event(Instant::now(), 1));
    assert_eq!(
        registry.snapshots()[0].degraded_reason.as_deref(),
        Some(NO_RECEIVER_AFTER_RESTART_REASON)
    );
}
