use super::*;
use crate::playback::state::PlayState;
use crate::playback::{PlaybackEngine, PlaybackEngineConfig};
use sp_core::ws::ServerMsg;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

async fn fresh_engine() -> (PlaybackEngine, Arc<NdiHealthRegistry>) {
    let pool = SqlitePool::connect(":memory:").await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let registry = Arc::new(NdiHealthRegistry::new());
    let engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: PathBuf::from("/tmp"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: registry.clone(),
    });
    (engine, registry)
}

/// Like `fresh_engine`, but wires a real `obs_cmd_tx` so tests can assert
/// the #127 receiver-recovery nudge command is dispatched.
async fn fresh_engine_with_obs_cmd() -> (
    PlaybackEngine,
    Arc<NdiHealthRegistry>,
    mpsc::Receiver<crate::obs::ObsCommand>,
) {
    let pool = SqlitePool::connect(":memory:").await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let (obs_cmd_tx, obs_cmd_rx) = mpsc::channel::<crate::obs::ObsCommand>(16);
    let registry = Arc::new(NdiHealthRegistry::new());
    let engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: PathBuf::from("/tmp"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: Some(obs_cmd_tx),
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: registry.clone(),
    });
    (engine, registry, obs_cmd_rx)
}

/// Build a dark-wall HealthSnapshot event (Playing, connections=0) with the
/// given consecutive-bad-poll count.
fn dark_wall_event(now: Instant, consecutive_bad_polls: u32) -> PipelineEvent {
    PipelineEvent::HealthSnapshot {
        connections: 0,
        frames_submitted_total: 12_000,
        frames_submitted_last_5s: 120,
        observed_fps: 30.0,
        nominal_fps: 30.0,
        last_submit_ts: Some(now),
        last_heartbeat_ts: now,
        consecutive_bad_polls,
        reported_state: PlaybackStateLabel::Playing,
        pacing: Default::default(),
        audio: Default::default(),
        loop_stats: Default::default(),
    }
}

/// #127: a Playing-on-program pipeline dark past the nudge threshold must
/// dispatch an OBS `NudgeNdiReceiver` for its stream. This is the RED test
/// for the receiver-recovery trigger — before the fix, nothing acted on the
/// dark-wall state SongPlayer already named.
#[tokio::test]
async fn handle_health_snapshot_nudges_obs_on_prolonged_dark_wall() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(4, true);

    let now = Instant::now();
    engine.handle_health_snapshot(
        4,
        dark_wall_event(now, crate::obs::ndi_recovery::NUDGE_THRESHOLD_BAD_POLLS),
    );

    // Rung 0 of the ladder (clear+restore) fires at the dark threshold.
    match obs_rx.try_recv() {
        Ok(crate::obs::ObsCommand::NudgeNdiReceiver { ndi_name, step }) => {
            assert_eq!(ndi_name, "SP-slow");
            assert_eq!(step, crate::obs::ndi_recovery::RecoveryStep::ClearRestore);
        }
        Ok(other) => panic!("expected NudgeNdiReceiver, got a different ObsCommand: {other:?}"),
        Err(e) => panic!("expected a NudgeNdiReceiver command, got none: {e:?}"),
    }
    // The fired rung is surfaced on the health snapshot for the dashboard / E2E.
    assert_eq!(
        registry.snapshots()[0].recovery_step,
        Some(crate::obs::ndi_recovery::RecoveryStep::ClearRestore),
        "the snapshot must record the rung that fired this poll",
    );
}

/// #173: a receiver that stays dark long enough for the ladder to walk
/// clear+restore → toggle → recreate must ESCALATE past the earlier rungs, and
/// the snapshot must record the fired rung. Rung 2 (`RecreateInput`) is enabled
/// again in round 3 (the executor creates-first-then-removes), so the highest
/// rung a sustained dark wall reaches is the recreate. Before the round-2 fix the
/// nudge repeated clear+restore forever and the wall stayed dark for ~20 min.
#[tokio::test]
async fn handle_health_snapshot_escalates_to_recreate_on_sustained_dark_wall() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(7, "SP-fast");
    engine.set_state_for_test(7, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(7, true);

    let base = crate::obs::ndi_recovery::NUDGE_THRESHOLD_BAD_POLLS;
    // Rung 0 (clear+restore) at the threshold, rung 1 (toggle) +2 dark polls
    // later, rung 2 (recreate) +2 more. Each poll climbs consecutive_bad_polls.
    let now = Instant::now();
    engine.handle_health_snapshot(7, dark_wall_event(now, base)); // rung 0
    engine.handle_health_snapshot(7, dark_wall_event(now, base + 2)); // rung 1
    engine.handle_health_snapshot(7, dark_wall_event(now, base + 4)); // rung 2

    // Drain the queued commands; the LAST one must be the recreate rung.
    let mut last_step = None;
    while let Ok(cmd) = obs_rx.try_recv() {
        if let crate::obs::ObsCommand::NudgeNdiReceiver { ndi_name, step } = cmd {
            assert_eq!(ndi_name, "SP-fast");
            last_step = Some(step);
        }
    }
    // #173 round 3: rung 2 (RecreateInput) is ENABLED — the executor now
    // creates-first-then-removes (a failed CreateInput can no longer empty the
    // scene), so a sustained dark wall escalates all the way to the recreate.
    assert_eq!(
        last_step,
        Some(crate::obs::ndi_recovery::RecoveryStep::RecreateInput),
        "a sustained dark wall must escalate the ladder to the recreate rung",
    );
    assert_eq!(
        registry.snapshots()[0].recovery_step,
        Some(crate::obs::ndi_recovery::RecoveryStep::RecreateInput),
        "the snapshot must record the escalated rung",
    );
}

/// A dark wall that recovers clears `recovery_step` back to `None` so the
/// dashboard / E2E stops showing a stale recovery rung.
#[tokio::test]
async fn handle_health_snapshot_clears_recovery_step_on_recovery() {
    let (mut engine, registry, _obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(4, true);

    let now = Instant::now();
    // Dark past threshold → rung 0 fires, recovery_step is Some.
    engine.handle_health_snapshot(
        4,
        dark_wall_event(now, crate::obs::ndi_recovery::NUDGE_THRESHOLD_BAD_POLLS),
    );
    assert!(registry.snapshots()[0].recovery_step.is_some());

    // Clean poll: receiver re-attached → recovery_step clears to None.
    engine.handle_health_snapshot(
        4,
        PipelineEvent::HealthSnapshot {
            connections: 2,
            frames_submitted_total: 12_100,
            frames_submitted_last_5s: 120,
            observed_fps: 30.0,
            nominal_fps: 30.0,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Playing,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );
    assert_eq!(
        registry.snapshots()[0].recovery_step,
        None,
        "a recovered receiver must clear the recovery_step",
    );
}

/// A dark wall below the nudge threshold (degraded, but only a couple of
/// bad polls) must NOT nudge OBS yet — a receiver that simply needs a moment
/// to connect is left alone.
#[tokio::test]
async fn handle_health_snapshot_does_not_nudge_below_threshold() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(4, true);

    let now = Instant::now();
    // 2 bad polls: degraded_reason IS set, but below the nudge threshold.
    engine.handle_health_snapshot(4, dark_wall_event(now, 2));

    assert_eq!(
        registry.snapshots()[0].degraded_reason.as_deref(),
        Some(DARK_WALL_REASON),
        "the dashboard degraded_reason must still fire below the nudge threshold",
    );
    assert!(
        obs_rx.try_recv().is_err(),
        "no nudge should be queued below the consecutive-poll threshold",
    );
}

#[tokio::test]
async fn handle_health_snapshot_populates_registry_for_known_pipeline() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(7, "SP-test");

    let now = Instant::now();
    engine.handle_health_snapshot(
        7,
        PipelineEvent::HealthSnapshot {
            connections: 2,
            frames_submitted_total: 150,
            frames_submitted_last_5s: 30,
            observed_fps: 29.97,
            nominal_fps: 29.97,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Playing,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );

    let snapshots = registry.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].playlist_id, 7);
    assert_eq!(snapshots[0].connections, 2);
    assert_eq!(snapshots[0].frames_submitted_total, 150);
    assert!(snapshots[0].last_submit_ts.is_some());
}

/// #198 item 5: the SYNC health handler must not `tokio::spawn` the receiver-
/// count DB persist — a spawn from a sync caller with no running reactor panics
/// ("there is no reactor running"). Build the engine (async, for the pool), then
/// call the sync handler OUTSIDE any runtime context: the pre-fix
/// `tokio::spawn(persist)` panicked here, so this is the RED test. A
/// connection-count change (None -> 2) is exactly the persist trigger.
#[test]
fn handle_health_snapshot_persists_without_a_tokio_reactor() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut engine = rt.block_on(async {
        let (mut engine, _registry) = fresh_engine().await;
        engine.ensure_pipeline(7, "SP-test");
        engine
    });

    let now = Instant::now();
    // NOT inside `rt` — no reactor is running here.
    engine.handle_health_snapshot(
        7,
        PipelineEvent::HealthSnapshot {
            connections: 2,
            frames_submitted_total: 150,
            frames_submitted_last_5s: 30,
            observed_fps: 29.97,
            nominal_fps: 29.97,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Playing,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );
    // Reaching here without a panic IS the assertion. Drop the engine (and its
    // sqlx pool) back inside the runtime so the pool teardown has a reactor.
    rt.block_on(async move { drop(engine) });
}

/// 0.62.0 release review: the drain → DB write in `handle_pipeline_event` had no
/// test (deleting the loop kept the workspace green while the #196 self-check
/// baseline was never written again). A changed count (None → 2) must land in
/// `settings` through the ASYNC event path.
#[tokio::test]
async fn health_snapshot_event_persists_the_receiver_count_to_the_db() {
    let (mut engine, _registry) = fresh_engine().await;
    engine.ensure_pipeline(7, "SP-test");
    let now = Instant::now();
    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::HealthSnapshot {
                connections: 2,
                frames_submitted_total: 150,
                frames_submitted_last_5s: 30,
                observed_fps: 29.97,
                nominal_fps: 29.97,
                last_submit_ts: Some(now),
                last_heartbeat_ts: now,
                consecutive_bad_polls: 0,
                reported_state: PlaybackStateLabel::Playing,
                pacing: Default::default(),
                audio: Default::default(),
                loop_stats: Default::default(),
            },
        )
        .await;
    let counts = crate::db::models_ndi::all_last_receiver_counts(&engine.pool).await;
    assert_eq!(counts.get(&7), Some(&2), "the changed count must be persisted");
}

/// #198 item 5: the pending receiver-count persist buffer debounces a flapping
/// count to its latest value and `drain` CLEARS it (so a value is written at
/// most once per drain). Kills the queue-noop / drain-empty / drain-no-clear
/// mutants.
#[test]
fn pending_receiver_count_persist_debounces_to_latest_and_drain_clears() {
    let reg = NdiHealthRegistry::new();
    reg.queue_receiver_count_persist(7, 2);
    reg.queue_receiver_count_persist(7, 0); // a flap within one drain — latest wins
    reg.queue_receiver_count_persist(9, 3);
    let mut drained = reg.drain_pending_persists();
    drained.sort();
    assert_eq!(drained, vec![(7, 0), (9, 3)]);
    // A second drain is empty — the buffer was taken, not cloned.
    assert!(reg.drain_pending_persists().is_empty());
}

#[tokio::test]
async fn handle_health_snapshot_drops_event_for_unknown_pipeline() {
    let (mut engine, registry) = fresh_engine().await;
    let now = Instant::now();
    engine.handle_health_snapshot(
        999,
        PipelineEvent::HealthSnapshot {
            connections: 0,
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 0.0,
            nominal_fps: 30.0,
            last_submit_ts: None,
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Idle,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );
    assert_eq!(registry.snapshots().len(), 0);
}

#[tokio::test]
async fn registry_holds_one_entry_per_pipeline_with_health() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(1, "SP-a");
    engine.ensure_pipeline(2, "SP-b");
    let now = Instant::now();
    let mk_event = |state| PipelineEvent::HealthSnapshot {
        connections: 1,
        frames_submitted_total: 0,
        frames_submitted_last_5s: 0,
        observed_fps: 0.0,
        nominal_fps: 30.0,
        last_submit_ts: None,
        last_heartbeat_ts: now,
        consecutive_bad_polls: 0,
        reported_state: state,
        pacing: Default::default(),
        audio: Default::default(),
        loop_stats: Default::default(),
    };
    engine.handle_health_snapshot(1, mk_event(PlaybackStateLabel::Playing));
    engine.handle_health_snapshot(2, mk_event(PlaybackStateLabel::Idle));
    let snapshots = registry.snapshots();
    assert_eq!(snapshots.len(), 2);
    let ids: Vec<_> = snapshots.iter().map(|s| s.playlist_id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
}

#[tokio::test]
async fn engine_overrides_idle_to_waiting_for_scene_when_canonical_state_says_so() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(5, "SP-w");
    engine.set_state_for_test(5, PlayState::WaitingForScene);

    let now = Instant::now();
    engine.handle_health_snapshot(
        5,
        PipelineEvent::HealthSnapshot {
            connections: 0,
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 0.0,
            nominal_fps: 30.0,
            last_submit_ts: None,
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Idle,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );

    let snapshots = registry.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].state,
        PlaybackStateLabel::WaitingForScene,
        "engine must override pipeline's Idle -> WaitingForScene when canonical state matches"
    );
}

#[tokio::test]
async fn handle_health_snapshot_fills_degraded_reason_at_2_consecutive_bad_polls() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(8, "SP-fail");
    engine.set_state_for_test(8, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(8, true);
    let now = Instant::now();
    engine.handle_health_snapshot(
        8,
        PipelineEvent::HealthSnapshot {
            connections: 0,
            frames_submitted_total: 100,
            frames_submitted_last_5s: 30,
            observed_fps: 30.0,
            nominal_fps: 30.0,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 2,
            reported_state: PlaybackStateLabel::Playing,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );
    let snapshots = registry.snapshots();
    assert_eq!(snapshots[0].consecutive_bad_polls, 2);
    assert_eq!(
        snapshots[0].degraded_reason.as_deref(),
        Some("no NDI receiver — wall is dark"),
    );
}

#[test]
fn degraded_reason_returns_none_at_one_bad_poll() {
    let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 0, 0.0, 30.0, 1);
    assert!(r.is_none(), "single bad poll must not trigger degradation");
}

#[test]
fn degraded_reason_returns_none_when_not_playing() {
    let r = compute_degraded_reason(&PlaybackStateLabel::Idle, 0, 0.0, 30.0, 5);
    assert!(r.is_none());
    let r = compute_degraded_reason(&PlaybackStateLabel::Paused, 0, 0.0, 30.0, 5);
    assert!(r.is_none());
    let r = compute_degraded_reason(&PlaybackStateLabel::WaitingForScene, 0, 0.0, 30.0, 5);
    assert!(r.is_none());
}

#[test]
fn degraded_reason_emits_underrun_when_fps_below_half_nominal() {
    let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 1, 10.0, 30.0, 2);
    assert_eq!(r.as_deref(), Some("underrunning (10/30 fps)"));
}

#[test]
fn degraded_reason_emits_stale_when_fps_ok_and_connections_ok() {
    let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 1, 30.0, 30.0, 2);
    assert_eq!(r.as_deref(), Some("no frames in 10s"));
}

/// Regression test for the 2026-04-27 production failure.
///
/// v0.25.0 deployed PR #58's Tier-2 RecreateSender as the auto-recovery
/// for prolonged `connections=0`. In production NDI's mDNS socket bound
/// to a stale APIPA address (`169.254.144.214`); per-sender recreate
/// could not fix that runtime-level binding, and `send_create` with the
/// existing name failed on the same-name conflict. The wall stayed dark
/// while the log spammed `RecreateSender mid-decode: failed; keeping existing`
/// every 30 s for ~50 minutes until the process was restarted.
///
/// v0.26.0 ripped the entire trigger out (no `RecreateSender` variant,
/// no `should_fire_recreate` predicate, no `recreate_attempts` snapshot
/// field) and reverted to Tier-1 visibility only. This test asserts the
/// remaining behaviour: prolonged `connections=0` while Playing fills
/// `degraded_reason` for the dashboard/log without any other side effects.
/// Re-introducing per-sender recreate machinery would have to redefine
/// the snapshot shape and is structurally caught by `cargo check` — but
/// this test is the documented contract.
#[tokio::test]
async fn handle_health_snapshot_visibility_only_on_prolonged_dark_wall() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(7, "SP-fast");
    engine.set_state_for_test(7, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(7, true);

    let now = Instant::now();
    // Simulate 100 consecutive bad polls (8+ minutes of dark wall) —
    // past every threshold the v0.25.0 PR #58 schedule fired at.
    engine.handle_health_snapshot(
        7,
        PipelineEvent::HealthSnapshot {
            connections: 0,
            frames_submitted_total: 12_000,
            frames_submitted_last_5s: 120,
            observed_fps: 24.0,
            nominal_fps: 24.0,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 100,
            reported_state: PlaybackStateLabel::Playing,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );

    let snap = &registry.snapshots()[0];
    assert_eq!(snap.consecutive_bad_polls, 100);
    assert_eq!(snap.connections, 0);
    // Tier-1 visibility fires.
    assert_eq!(
        snap.degraded_reason.as_deref(),
        Some("no NDI receiver — wall is dark"),
    );
}

/// Tier-1 visibility must clear when the wall recovers (e.g. operator
/// restarts SongPlayer after NDI APIPA binding made connections=0). A
/// clean poll after a degraded run drops `degraded_reason` back to None
/// so the dashboard / log "ndi: pipeline recovered" path fires.
#[tokio::test]
async fn handle_health_snapshot_clears_degraded_reason_on_clean_poll() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(7, "SP-fast");
    engine.set_state_for_test(7, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(7, true);

    let now = Instant::now();
    // First: degraded.
    engine.handle_health_snapshot(
        7,
        PipelineEvent::HealthSnapshot {
            connections: 0,
            frames_submitted_total: 240,
            frames_submitted_last_5s: 120,
            observed_fps: 24.0,
            nominal_fps: 24.0,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 5,
            reported_state: PlaybackStateLabel::Playing,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );
    assert_eq!(
        registry.snapshots()[0].degraded_reason.as_deref(),
        Some("no NDI receiver — wall is dark")
    );

    // Then: clean poll. Connections returned, no consecutive_bad_polls.
    engine.handle_health_snapshot(
        7,
        PipelineEvent::HealthSnapshot {
            connections: 2,
            frames_submitted_total: 480,
            frames_submitted_last_5s: 120,
            observed_fps: 24.0,
            nominal_fps: 24.0,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 0,
            reported_state: PlaybackStateLabel::Playing,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );
    let snap = &registry.snapshots()[0];
    assert_eq!(snap.connections, 2);
    assert_eq!(snap.consecutive_bad_polls, 0);
    assert!(
        snap.degraded_reason.is_none(),
        "clean poll must clear degraded_reason so 'ndi: pipeline recovered' log fires",
    );
}

#[test]
fn should_log_periodic_heartbeat_on_first_heartbeat() {
    let cur: DateTime<Utc> = "2026-04-28T05:21:00Z".parse().unwrap();
    assert!(
        should_log_periodic_heartbeat(None, cur),
        "first heartbeat for a pipeline must always log"
    );
}

#[test]
fn should_log_periodic_heartbeat_on_new_minute_bucket() {
    let prev: DateTime<Utc> = "2026-04-28T05:21:55Z".parse().unwrap();
    let cur: DateTime<Utc> = "2026-04-28T05:22:00Z".parse().unwrap();
    assert!(
        should_log_periodic_heartbeat(Some(prev), cur),
        "crossing into a new UTC-minute bucket must log"
    );
}

#[test]
fn should_log_periodic_heartbeat_suppresses_within_same_minute() {
    let prev: DateTime<Utc> = "2026-04-28T05:21:00Z".parse().unwrap();
    let cur: DateTime<Utc> = "2026-04-28T05:21:55Z".parse().unwrap();
    assert!(
        !should_log_periodic_heartbeat(Some(prev), cur),
        "heartbeats inside the same UTC minute must NOT spam the log"
    );
}

#[tokio::test]
async fn handle_health_snapshot_skips_alert_when_scene_inactive() {
    // Pipeline is decoding (state=Playing) but OBS is on a different
    // scene → scene_active=false. Even with connections=0, no alert.
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(9, "SP-off");
    engine.set_state_for_test(9, PlayState::Playing { video_id: 1 });
    // scene_active defaults to false on a fresh pipeline; do not flip it.

    let now = Instant::now();
    engine.handle_health_snapshot(
        9,
        PipelineEvent::HealthSnapshot {
            connections: 0,
            frames_submitted_total: 100,
            frames_submitted_last_5s: 30,
            observed_fps: 30.0,
            nominal_fps: 30.0,
            last_submit_ts: Some(now),
            last_heartbeat_ts: now,
            consecutive_bad_polls: 5,
            reported_state: PlaybackStateLabel::Playing,
            pacing: Default::default(),
            audio: Default::default(),
            loop_stats: Default::default(),
        },
    );
    let snapshots = registry.snapshots();
    assert_eq!(snapshots[0].state, PlaybackStateLabel::Paused);
    assert!(
        snapshots[0].degraded_reason.is_none(),
        "scene_active=false must not produce a degraded_reason even with connections=0"
    );
}

// ---- #167 registry readiness signals -------------------------------------

#[test]
fn register_pipeline_increments_created_count() {
    let reg = NdiHealthRegistry::new();
    assert_eq!(
        reg.created_pipelines(),
        0,
        "fresh registry has no pipelines"
    );
    reg.register_pipeline();
    assert_eq!(reg.created_pipelines(), 1);
    reg.register_pipeline();
    reg.register_pipeline();
    assert_eq!(reg.created_pipelines(), 3);
}

#[test]
fn reported_pipelines_counts_distinct_seeded_snapshots() {
    let reg = NdiHealthRegistry::new();
    assert_eq!(reg.reported_pipelines(), 0, "no heartbeats yet");
    reg.update(mk_reported_snapshot(7));
    assert_eq!(reg.reported_pipelines(), 1);
    // A second distinct pipeline reporting bumps the count.
    reg.update(mk_reported_snapshot(9));
    assert_eq!(reg.reported_pipelines(), 2);
    // A re-report of an existing pipeline does NOT double-count.
    reg.update(mk_reported_snapshot(7));
    assert_eq!(reg.reported_pipelines(), 2);
}

// ---- #196 post-restart receiver self-check registry state ----------------

#[test]
fn seeded_pre_restart_count_is_read_back_else_zero() {
    let reg = NdiHealthRegistry::new();
    assert_eq!(reg.pre_restart_count(4), 0, "unseeded output reads 0");
    let mut baseline = std::collections::HashMap::new();
    baseline.insert(4, 2);
    baseline.insert(9, 0);
    reg.seed_pre_restart_counts(baseline);
    assert_eq!(reg.pre_restart_count(4), 2, "seeded value read back");
    assert_eq!(reg.pre_restart_count(9), 0);
    assert_eq!(reg.pre_restart_count(99), 0, "unknown output still 0");
}

#[test]
fn senders_ready_gates_elapsed_since_ready() {
    let reg = NdiHealthRegistry::new();
    assert!(
        reg.elapsed_since_ready().is_none(),
        "no elapsed before the senders are marked ready"
    );
    reg.mark_senders_ready();
    assert!(
        reg.elapsed_since_ready().is_some(),
        "elapsed is Some once ready"
    );
}

#[test]
fn reconnected_latch_records_per_output() {
    let reg = NdiHealthRegistry::new();
    assert!(!reg.has_reconnected(4));
    reg.mark_reconnected(4);
    assert!(reg.has_reconnected(4));
    assert!(!reg.has_reconnected(9), "a different output is independent");
}

#[test]
fn warned_no_receiver_fires_once_then_clears() {
    let reg = NdiHealthRegistry::new();
    assert!(
        reg.mark_warned_no_receiver(4),
        "first WARN for an output returns true"
    );
    assert!(
        !reg.mark_warned_no_receiver(4),
        "a second WARN for the same output returns false (once per output)"
    );
    reg.clear_warned_no_receiver(4);
    assert!(
        reg.mark_warned_no_receiver(4),
        "after recovery clears the latch, a new failure warns again"
    );
}

/// Minimal seeded snapshot for the readiness-count tests — every field zeroed
/// except the identity, so `reported_pipelines()` (a map-len read) can be
/// exercised without the full engine heartbeat path.
fn mk_reported_snapshot(playlist_id: i64) -> PipelineHealthSnapshot {
    PipelineHealthSnapshot {
        playlist_id,
        ndi_name: format!("SP-{playlist_id}"),
        state: PlaybackStateLabel::Idle,
        connections: 0,
        frames_submitted_total: 0,
        frames_submitted_last_5s: 0,
        observed_fps: 0.0,
        nominal_fps: 0.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: crate::playback::clock_health::ClockHealth::default(),
        pacing: Default::default(),
        audio: Default::default(),
        lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
        lock_reason: String::new(),
        burn_on: false,
        recovery_step: None,
        sender_url: None,
    }
}

// #196: effective_dark_reason — a dark output with NO OBS input advertising it
// gets the no-input reason (not the dark-wall reason), so the ladder is skipped.
#[test]
fn effective_dark_reason_no_input_dark_wall_becomes_no_obs_input() {
    assert_eq!(
        effective_dark_reason(Some(DARK_WALL_REASON.to_string()), false).as_deref(),
        Some(NO_OBS_INPUT_REASON)
    );
}

#[test]
fn effective_dark_reason_dark_wall_with_input_passes_through() {
    assert_eq!(
        effective_dark_reason(Some(DARK_WALL_REASON.to_string()), true).as_deref(),
        Some(DARK_WALL_REASON)
    );
}

#[test]
fn effective_dark_reason_none_stays_none_even_without_input() {
    assert_eq!(effective_dark_reason(None, false), None);
}

#[test]
fn effective_dark_reason_other_reason_passes_through() {
    assert_eq!(
        effective_dark_reason(Some("stalled".to_string()), false).as_deref(),
        Some("stalled")
    );
}

// #196: the registry records a sender's advertised URL at creation and reads
// it back onto every snapshot.
#[test]
fn registry_records_and_reads_sender_url() {
    let reg = NdiHealthRegistry::new();
    assert_eq!(reg.sender_url(7), None);
    reg.set_sender_url(7, Some("10.77.9.201:5963".to_string()));
    assert_eq!(reg.sender_url(7).as_deref(), Some("10.77.9.201:5963"));
    // A different id is independent.
    assert_eq!(reg.sender_url(8), None);
}

#[test]
fn registry_set_sender_url_none_keeps_prior() {
    let reg = NdiHealthRegistry::new();
    reg.set_sender_url(7, Some("10.77.9.201:5963".to_string()));
    reg.set_sender_url(7, None);
    assert_eq!(reg.sender_url(7).as_deref(), Some("10.77.9.201:5963"));
}

// #196: output_has_obs_input reflects the shared OBS source map — no map wired
// reads as "has input" (never suppress the ladder without evidence); a wired
// map answers by playlist_id membership.
#[tokio::test]
async fn output_has_obs_input_reflects_source_map() {
    let (mut engine, _reg) = fresh_engine().await;
    // No map wired (OBS not configured) → treated as "has input".
    assert!(engine.output_has_obs_input(7));

    let map: crate::obs::NdiSourceMap =
        std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    map.write().await.insert("sp-fast_video".to_string(), 7);
    engine.set_ndi_source_map(map);

    assert!(
        engine.output_has_obs_input(7),
        "an OBS input advertises playlist 7"
    );
    assert!(
        !engine.output_has_obs_input(999),
        "no OBS input advertises playlist 999"
    );
}
