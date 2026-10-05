//! #221 B4 step 6: no receiver is expected on a playlist's own NDI output
//! (`ndi_health_expect::PLAYLIST_RECEIVER_EXPECTED`): its receiver count
//! plays no part in its health (review round 1). No dark-wall reason, no #173
//! ladder rung, no #196 post-restart flag and no "no receiver" lock for it;
//! an underrun or a stalled submit is still reported. The state label stays
//! keyed on on-air. Shares the rig of `ndi_health_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "ndi_health_tests_expect.rs"] mod tests_expect;`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use sp_core::genlock::lock_state::LockState;
use sp_core::health::SELF_CHECK_DELAY;

use super::PlaybackStateLabel;
use super::tests::{dark_wall_event, fresh_engine_with_obs_cmd};
use crate::playback::clock_health::{DantesyncStatus, evaluate};
use crate::playback::pipeline::PipelineEvent;
use crate::playback::state::PlayState;

/// A playlist on air (playing, on program) with 0 receivers, as the pipeline
/// reports it: no bad poll (a missing receiver is none, `classify_bad_poll`).
/// It is healthy — no degraded reason, no ladder rung, no OBS command —
/// while the label still reads Playing (the badge and the idle gates are
/// on-air).
#[tokio::test]
async fn a_playlist_output_on_air_expects_no_receiver() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let now = Instant::now();
    for _ in 0..3 {
        engine.handle_health_snapshot(4, dark_wall_event(now, 0));
    }
    let snap = registry.snapshots()[0].clone();
    assert_eq!(snap.state, PlaybackStateLabel::Playing);
    assert_eq!(snap.connections, 0, "the snapshot keeps the real count");
    assert_eq!(snap.degraded_reason, None);
    assert_eq!(snap.recovery_step, None);
    assert!(
        obs_rx.try_recv().is_err(),
        "no rung ran against cg OBS's inputs"
    );
}

/// The genlock badge: with the clock and the pacing fine, a playlist
/// output's first heartbeat on a clean grid is LOCKED with 0 receivers —
/// never DEGRADED "no receiver" (before, every playlist output cg OBS no
/// longer showed read DEGRADED).
#[tokio::test]
async fn a_playlist_output_s_lock_never_degrades_for_no_receiver() {
    let (mut engine, registry, _obs_rx) = fresh_engine_with_obs_cmd().await;
    let clock = evaluate(Some(&DantesyncStatus {
        is_locked: Some(true),
        mode: Some("NANO".to_string()),
        offset_ns: None,
        ntp_failed: None,
        ntp_age_s: None,
    }));
    assert!(clock.clock_ok, "the rig's clock is fine");
    engine.set_clock_health(Arc::new(RwLock::new(clock)));
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let mut poll = dark_wall_event(Instant::now(), 0);
    if let PipelineEvent::HealthSnapshot { pacing, .. } = &mut poll {
        pacing.enabled = true;
    }
    engine.handle_health_snapshot(4, poll);
    let snap = registry.snapshots()[0].clone();
    assert_eq!(snap.connections, 0);
    assert_eq!(
        (snap.lock_state, snap.lock_reason.as_str()),
        (LockState::Locked, "locked")
    );
}

/// An on-air output that underruns is degraded with 0 receivers too
/// (`SP-program` takes its frames), and it runs no ladder rung. Before, the
/// dark-wall reason came first at 0 receivers and was then dropped, so the
/// underrun was never reported.
#[tokio::test]
async fn an_underrun_is_still_reported_on_a_playlist_output() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let mut underrun = dark_wall_event(Instant::now(), 2);
    if let PipelineEvent::HealthSnapshot { observed_fps, .. } = &mut underrun {
        *observed_fps = 10.0; // below half of the nominal 30
    }
    engine.handle_health_snapshot(4, underrun);
    let snap = registry.snapshots()[0].clone();
    assert_eq!(snap.connections, 0);
    assert_eq!(
        snap.degraded_reason.as_deref(),
        Some("underrunning (10/30 fps)")
    );
    assert_eq!(snap.recovery_step, None);
    assert!(obs_rx.try_recv().is_err(), "an underrun is no dark wall");
}

/// A stalled submit (two bad polls, the last frame 11 s ago) is "no frames
/// in 10s" with 0 receivers: a bad poll at full rate is a stall now.
#[tokio::test]
async fn a_stalled_playlist_output_is_reported_with_no_receiver() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let now = Instant::now();
    let mut stalled = dark_wall_event(now, 2);
    if let PipelineEvent::HealthSnapshot { last_submit_ts, .. } = &mut stalled {
        *last_submit_ts = now.checked_sub(Duration::from_secs(11));
    }
    engine.handle_health_snapshot(4, stalled);
    let snap = registry.snapshots()[0].clone();
    assert_eq!(snap.degraded_reason.as_deref(), Some("no frames in 10s"));
    assert_eq!(snap.recovery_step, None);
    assert!(obs_rx.try_recv().is_err(), "a stall is no dark wall");
}

/// #196's post-restart self-check never flags a playlist output: 30 s after
/// the senders were ready, one that had receivers before the restart (cg
/// OBS showed it) and has none now is normal — no receiver is expected on
/// it any more.
#[tokio::test]
async fn the_post_restart_self_check_never_flags_a_playlist_output() {
    let (mut engine, registry, _obs_rx) = fresh_engine_with_obs_cmd().await;
    let ready = Instant::now().checked_sub(SELF_CHECK_DELAY + Duration::from_secs(1));
    *registry.senders_ready_at.write().unwrap() = ready;
    registry.seed_pre_restart_counts(HashMap::from([(4, 2)]));
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);

    engine.handle_health_snapshot(4, dark_wall_event(Instant::now(), 0));
    assert_eq!(
        registry.snapshots()[0].degraded_reason,
        None,
        "it had receivers before the restart, but none is expected now"
    );
}
