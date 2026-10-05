//! #221 B4 step 6: no receiver is expected on a playlist's own NDI output
//! (`ndi_health_expect::PLAYLIST_RECEIVER_EXPECTED`): the dark-wall reason
//! and the #173 ladder never fire on it, an underrun is still reported, and
//! the #196 post-restart self-check flags only an output that had a receiver
//! before the restart. The state label stays keyed on on-air. Shares the rig
//! of `ndi_health_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "ndi_health_tests_expect.rs"] mod tests_expect;`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sp_core::health::{NO_RECEIVER_AFTER_RESTART_REASON, SELF_CHECK_DELAY};

use super::PlaybackStateLabel;
use super::tests::{dark_wall_event, fresh_engine_with_obs_cmd};
use crate::obs::ndi_recovery::NUDGE_THRESHOLD_BAD_POLLS;
use crate::playback::pipeline::PipelineEvent;
use crate::playback::state::PlayState;

/// A playlist on air (playing, its scene on program) with 0 receivers: no
/// degraded reason, no ladder rung, no OBS command — while the label still
/// reads Playing (the badge and the idle gates are on-air).
#[tokio::test]
async fn a_playlist_output_on_air_expects_no_receiver() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let now = Instant::now();
    for polls in [2, NUDGE_THRESHOLD_BAD_POLLS, NUDGE_THRESHOLD_BAD_POLLS + 4] {
        engine.handle_health_snapshot(4, dark_wall_event(now, polls));
    }
    let snap = registry.snapshots()[0].clone();
    assert_eq!(snap.state, PlaybackStateLabel::Playing);
    assert_eq!(snap.degraded_reason, None);
    assert_eq!(snap.recovery_step, None);
    assert!(
        obs_rx.try_recv().is_err(),
        "no rung ran against cg OBS's inputs"
    );
}

/// Only the DARK-WALL reason is dropped: an on-air output that underruns is
/// degraded (`SP-program` takes its frames), and it runs no ladder rung.
#[tokio::test]
async fn an_underrun_is_still_reported_on_a_playlist_output() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let mut underrun = dark_wall_event(Instant::now(), 2);
    if let PipelineEvent::HealthSnapshot {
        connections,
        observed_fps,
        ..
    } = &mut underrun
    {
        *connections = 1;
        *observed_fps = 10.0; // below half of the nominal 30
    }
    engine.handle_health_snapshot(4, underrun);
    let snap = registry.snapshots()[0].clone();
    assert_eq!(
        snap.degraded_reason.as_deref(),
        Some("underrunning (10/30 fps)")
    );
    assert_eq!(snap.recovery_step, None);
    assert!(obs_rx.try_recv().is_err(), "an underrun is no dark wall");
}

/// #196's post-restart self-check: 30 s after the senders were ready, an
/// on-air playlist output with no receiver is flagged only when it had one
/// before the restart (no receiver is expected on it for being on air).
#[tokio::test]
async fn the_post_restart_self_check_flags_only_an_output_that_had_a_receiver_before() {
    let (mut engine, registry, _obs_rx) = fresh_engine_with_obs_cmd().await;
    let ready = Instant::now().checked_sub(SELF_CHECK_DELAY + Duration::from_secs(1));
    *registry.senders_ready_at.write().unwrap() = ready;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);

    // One bad poll: below the dark-wall gate, so only the self-check speaks.
    engine.handle_health_snapshot(4, dark_wall_event(Instant::now(), 1));
    assert_eq!(
        registry.snapshots()[0].degraded_reason,
        None,
        "on air, but no receiver is expected and it had none before"
    );

    registry.seed_pre_restart_counts(HashMap::from([(4, 2)]));
    engine.handle_health_snapshot(4, dark_wall_event(Instant::now(), 1));
    assert_eq!(
        registry.snapshots()[0].degraded_reason.as_deref(),
        Some(NO_RECEIVER_AFTER_RESTART_REASON),
        "it had receivers before the restart"
    );
}
