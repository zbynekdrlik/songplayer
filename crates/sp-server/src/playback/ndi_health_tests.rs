use super::*;
use crate::playback::state::PlayState;
use crate::playback::{PlaybackEngine, PlaybackEngineConfig};
use sp_core::ws::ServerMsg;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

pub(super) async fn fresh_engine() -> (PlaybackEngine, Arc<NdiHealthRegistry>) {
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

/// Like `fresh_engine`, but wires a real `obs_cmd_tx` so a test can assert
/// that nothing is sent to cg OBS (#221 lane 3: the #127/#173 receiver
/// ladder is deleted with the per-playlist senders).
pub(super) async fn fresh_engine_with_obs_cmd() -> (
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

/// A heartbeat of a playlist that is playing, as the pipeline reports it,
/// with the given consecutive-bad-poll count.
pub(super) fn playing_event(now: Instant, consecutive_bad_polls: u32) -> PipelineEvent {
    PipelineEvent::HealthSnapshot {
        frames_submitted_total: 12_000,
        frames_submitted_last_5s: 120,
        observed_fps: 30.0,
        nominal_fps: 30.0,
        source_fps: 30.0,
        last_submit_ts: Some(now),
        last_heartbeat_ts: now,
        consecutive_bad_polls,
        reported_state: PlaybackStateLabel::Playing,
        pacing: Default::default(),
        audio: Default::default(),
        loop_stats: Default::default(),
    }
}

/// #221 lane 3: a playlist has NO NDI output of its own, so a health row
/// carries no receiver count, no sender URL, no burn flag and no recovery
/// rung — the keys are gone from `GET /api/v1/ndi/health`, and the row
/// still names the playlist's output label and its paced delivery.
#[tokio::test]
async fn a_health_row_carries_no_ndi_sender_fields() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    engine.handle_health_snapshot(4, playing_event(Instant::now(), 0));

    let row = serde_json::to_value(&registry.snapshots()[0]).unwrap();
    let obj = row.as_object().expect("a row is an object");
    for gone in ["connections", "sender_url", "burn_on", "recovery_step"] {
        assert!(!obj.contains_key(gone), "`{gone}` is a sender field: {row}");
    }
    assert_eq!(row["ndi_name"], "SP-slow");
    assert_eq!(row["state"], "Playing");
    assert_eq!(row["frames_submitted_total"], 12_000);
    assert_eq!(row["degraded_reason"], serde_json::Value::Null);
}

/// A playlist on air with a run of bad polls (an underrun, as the pipeline
/// counts them) is named by that underrun, at any length of the run — never
/// a "dark wall": there is no receiver to miss (#221 lane 3), and nothing
/// is sent to cg OBS's inputs (the #173 ladder is deleted). The snapshot
/// keeps the bad polls (visibility only; #60: never a per-sender recreate).
#[tokio::test]
async fn a_run_of_bad_polls_on_air_is_named_by_its_underrun() {
    let (mut engine, registry, mut obs_rx) = fresh_engine_with_obs_cmd().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);

    let now = Instant::now();
    for polls in [2, 6, 8, 10, 100] {
        let mut underrun = playing_event(now, polls);
        if let PipelineEvent::HealthSnapshot { observed_fps, .. } = &mut underrun {
            *observed_fps = 10.0; // below half of the nominal 30
        }
        engine.handle_health_snapshot(4, underrun);
    }
    let snap = registry.snapshots()[0].clone();
    assert_eq!(snap.state, PlaybackStateLabel::Playing, "it is on program");
    assert_eq!(snap.consecutive_bad_polls, 100);
    assert_eq!(
        snap.degraded_reason.as_deref(),
        Some("underrunning (10/30 fps)"),
    );
    assert!(obs_rx.try_recv().is_err(), "nothing was sent to cg OBS");
}

/// A playlist on air with clean polls is healthy and reads Playing (the
/// badge and the #154/#167 idle gates are keyed on air).
#[tokio::test]
async fn a_playlist_on_air_with_clean_polls_is_healthy() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let now = Instant::now();
    for _ in 0..3 {
        engine.handle_health_snapshot(4, playing_event(now, 0));
    }
    let snap = registry.snapshots()[0].clone();
    assert_eq!(snap.state, PlaybackStateLabel::Playing);
    assert_eq!(snap.degraded_reason, None);
}

/// A stalled delivery (two bad polls at full rate, the last frame 11 s ago)
/// is "no frames in 10s".
#[tokio::test]
async fn a_stalled_playlist_on_air_is_reported() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let now = Instant::now();
    let mut stalled = playing_event(now, 2);
    if let PipelineEvent::HealthSnapshot { last_submit_ts, .. } = &mut stalled {
        *last_submit_ts = now.checked_sub(std::time::Duration::from_secs(11));
    }
    engine.handle_health_snapshot(4, stalled);
    let snap = registry.snapshots()[0].clone();
    assert_eq!(snap.degraded_reason.as_deref(), Some("no frames in 10s"));
}

/// The genlock badge: with the clock and the pacing fine, a playlist's first
/// heartbeat on a clean grid is LOCKED (#221 lane 3: no "no receiver" rule —
/// before B4 step 6, every playlist output cg OBS no longer showed read
/// DEGRADED for it).
#[tokio::test]
async fn a_playlist_s_lock_reads_locked_on_a_clean_grid() {
    let (mut engine, registry) = fresh_engine().await;
    let clock = crate::playback::clock_health::evaluate(Some(
        &crate::playback::clock_health::DantesyncStatus {
            is_locked: Some(true),
            mode: Some("NANO".to_string()),
            offset_ns: None,
            ntp_failed: None,
            ntp_age_s: None,
        },
    ));
    assert!(clock.clock_ok, "the rig's clock is fine");
    engine.set_clock_health(Arc::new(std::sync::RwLock::new(clock)));
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(4);
    let mut poll = playing_event(Instant::now(), 0);
    if let PipelineEvent::HealthSnapshot { pacing, .. } = &mut poll {
        pacing.enabled = true;
    }
    engine.handle_health_snapshot(4, poll);
    let snap = registry.snapshots()[0].clone();
    assert_eq!(
        (snap.lock_state, snap.lock_reason.as_str()),
        (sp_core::genlock::lock_state::LockState::Locked, "locked")
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
            frames_submitted_total: 150,
            frames_submitted_last_5s: 30,
            observed_fps: 29.97,
            nominal_fps: 29.97,
            source_fps: 29.97,
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
    assert_eq!(snapshots[0].frames_submitted_total, 150);
    assert!(snapshots[0].last_submit_ts.is_some());
}

/// #198 item 5: the SYNC health handler must never need a tokio reactor (a
/// `tokio::spawn` from a sync caller with no running reactor panics: "there
/// is no reactor running"). Build the engine (async, for the pool), then
/// call the sync handler OUTSIDE any runtime context. (#221 lane 3 deleted
/// the receiver-count persist this test was written for; the handler stays
/// sync.)
#[test]
fn handle_health_snapshot_runs_without_a_tokio_reactor() {
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
            frames_submitted_total: 150,
            frames_submitted_last_5s: 30,
            observed_fps: 29.97,
            nominal_fps: 29.97,
            source_fps: 29.97,
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

/// #201 (0.62.0 review): a manual /play on a pipeline that is ALREADY Playing
/// must be a no-op — the scene-on fallback would flag an off-program output as
/// on program. #221 L4b: a ▶ never flags a pipeline on program at all (the
/// playback authority does): the not-playing branch starts it and leaves it
/// off program too. Uses `fresh_engine` from this file.
#[tokio::test]
async fn engine_play_never_flags_a_pipeline_on_program() {
    let (mut engine, _registry) = fresh_engine().await;
    engine.ensure_pipeline(7, "SP-test");
    use std::sync::atomic::Ordering;
    {
        let pp = engine.pipelines.get_mut(&7).unwrap();
        pp.state = crate::playback::state::PlayState::Playing { video_id: 42 };
        pp.scene_active.store(false, Ordering::Release);
    }
    engine.handle_engine_play(7).await;
    assert!(
        !engine
            .pipelines
            .get(&7)
            .unwrap()
            .scene_active
            .load(Ordering::Acquire),
        "a playing pipeline must not be flagged on program by /play"
    );

    // The opposite branch starts it (no song in this DB) and claims nothing.
    engine.pipelines.get_mut(&7).unwrap().state =
        crate::playback::state::PlayState::WaitingForScene;
    engine.handle_engine_play(7).await;
    assert!(
        !engine
            .pipelines
            .get(&7)
            .unwrap()
            .scene_active
            .load(Ordering::Acquire),
        "a ▶ never flags a pipeline on program (#221 L4b)"
    );
}

#[tokio::test]
async fn handle_health_snapshot_drops_event_for_unknown_pipeline() {
    let (mut engine, registry) = fresh_engine().await;
    let now = Instant::now();
    engine.handle_health_snapshot(
        999,
        PipelineEvent::HealthSnapshot {
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 0.0,
            nominal_fps: 30.0,
            source_fps: 30.0,
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
        frames_submitted_total: 0,
        frames_submitted_last_5s: 0,
        observed_fps: 0.0,
        nominal_fps: 30.0,
        source_fps: 30.0,
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
            frames_submitted_total: 0,
            frames_submitted_last_5s: 0,
            observed_fps: 0.0,
            nominal_fps: 30.0,
            source_fps: 30.0,
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

/// The ≥ 2 consecutive bad polls gate still names a playlist output's own
/// faults (#221 B4 step 6: an underrun; never a missing receiver).
#[tokio::test]
async fn handle_health_snapshot_fills_degraded_reason_at_2_consecutive_bad_polls() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(8, "SP-fail");
    engine.set_state_for_test(8, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(8);
    let now = Instant::now();
    engine.handle_health_snapshot(
        8,
        PipelineEvent::HealthSnapshot {
            frames_submitted_total: 100,
            frames_submitted_last_5s: 30,
            observed_fps: 10.0,
            nominal_fps: 30.0,
            source_fps: 30.0,
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
        Some("underrunning (10/30 fps)"),
    );
}

#[test]
fn degraded_reason_returns_none_at_one_bad_poll() {
    let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 0.0, 30.0, 1);
    assert!(r.is_none(), "single bad poll must not trigger degradation");
}

#[test]
fn degraded_reason_returns_none_when_not_playing() {
    let r = compute_degraded_reason(&PlaybackStateLabel::Idle, 0.0, 30.0, 5);
    assert!(r.is_none());
    let r = compute_degraded_reason(&PlaybackStateLabel::Paused, 0.0, 30.0, 5);
    assert!(r.is_none());
    let r = compute_degraded_reason(&PlaybackStateLabel::WaitingForScene, 0.0, 30.0, 5);
    assert!(r.is_none());
}

#[test]
fn degraded_reason_emits_underrun_when_fps_below_half_nominal() {
    let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 10.0, 30.0, 2);
    assert_eq!(r.as_deref(), Some("underrunning (10/30 fps)"));
}

#[test]
fn degraded_reason_emits_stale_when_fps_ok() {
    let r = compute_degraded_reason(&PlaybackStateLabel::Playing, 30.0, 30.0, 2);
    assert_eq!(r.as_deref(), Some("no frames in 10s"));
}

/// A degraded reason clears on a clean poll, so the "ndi: pipeline
/// recovered" log fires (an underrun here).
#[tokio::test]
async fn handle_health_snapshot_clears_degraded_reason_on_clean_poll() {
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(7, "SP-fast");
    engine.set_state_for_test(7, PlayState::Playing { video_id: 1 });
    engine.set_on_program_for_test(7);

    let now = Instant::now();
    // First: degraded (an underrun).
    engine.handle_health_snapshot(
        7,
        PipelineEvent::HealthSnapshot {
            frames_submitted_total: 240,
            frames_submitted_last_5s: 50,
            observed_fps: 10.0,
            nominal_fps: 24.0,
            source_fps: 24.0,
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
        Some("underrunning (10/24 fps)")
    );

    // Then: clean poll. Full rate, no consecutive_bad_polls.
    engine.handle_health_snapshot(
        7,
        PipelineEvent::HealthSnapshot {
            frames_submitted_total: 480,
            frames_submitted_last_5s: 120,
            observed_fps: 24.0,
            nominal_fps: 24.0,
            source_fps: 24.0,
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
    // Pipeline is decoding (state=Playing) but it is not on program →
    // scene_active=false. Even with a run of bad polls, no alert.
    let (mut engine, registry) = fresh_engine().await;
    engine.ensure_pipeline(9, "SP-off");
    engine.set_state_for_test(9, PlayState::Playing { video_id: 1 });
    // scene_active defaults to false on a fresh pipeline; do not flip it.

    let now = Instant::now();
    engine.handle_health_snapshot(
        9,
        PipelineEvent::HealthSnapshot {
            frames_submitted_total: 100,
            frames_submitted_last_5s: 30,
            observed_fps: 30.0,
            nominal_fps: 30.0,
            source_fps: 30.0,
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
        "scene_active=false must not produce a degraded_reason"
    );
}

/// #168 r6b — a paced HealthSnapshot event with distinct `nominal_fps` (grid) + `source_fps` (decoder).
fn paced_lock_event(
    ts: Instant,
    nominal: f32,
    source: f32,
    seq: u64,
    late: u64,
    reps: u64,
) -> PipelineEvent {
    PipelineEvent::HealthSnapshot {
        frames_submitted_total: seq,
        frames_submitted_last_5s: 30,
        observed_fps: 30.0,
        nominal_fps: nominal,
        source_fps: source,
        last_submit_ts: Some(ts),
        last_heartbeat_ts: ts,
        consecutive_bad_polls: 0,
        reported_state: PlaybackStateLabel::Playing,
        pacing: PacingStats {
            enabled: true,
            seq,
            late_frames: late,
            repeats: reps,
            resyncs: 0,
            ..Default::default()
        },
        audio: Default::default(),
        loop_stats: Default::default(),
    }
}

/// #168 r6b — the fix at the engine seam. A paced 23.976-fps output on the 30-grid
/// (box read: slots 1803, late 3, repeats 362) reports `nominal_fps = 30` but
/// `source_fps = 23.976`; `handle_health_snapshot` must copy `source_fps` onto the
/// snapshot AND feed it (not the grid nominal) to the lock rule → LOCKED.
#[tokio::test]
async fn handle_health_snapshot_locks_on_source_fps_not_grid_nominal() {
    let (mut engine, registry) = fresh_engine().await;
    // Box clock LOCKED (NANO) so the lock rule reaches the rate checks.
    let clock = crate::playback::clock_health::evaluate(Some(
        &crate::playback::clock_health::DantesyncStatus {
            is_locked: Some(true),
            mode: Some("NANO".to_string()),
            offset_ns: None,
            ntp_failed: None,
            ntp_age_s: None,
        },
    ));
    engine.set_clock_health(Arc::new(std::sync::RwLock::new(clock)));
    engine.ensure_pipeline(4, "SP-slow");
    engine.set_state_for_test(4, PlayState::Playing { video_id: 1 });
    engine.set_scene_active_for_test(4, true);

    // Two heartbeats 60 s apart build the differenced window: baseline then counts.
    let t0 = Instant::now();
    let t1 = t0 + std::time::Duration::from_secs(60);
    engine.handle_health_snapshot(4, paced_lock_event(t0, 30.0, 23.976, 0, 0, 0));
    engine.handle_health_snapshot(4, paced_lock_event(t1, 30.0, 23.976, 1803, 3, 362));

    let snap = &registry.snapshots()[0];
    // (a) source_fps survives event → snapshot; nominal_fps stays the grid.
    assert_eq!(
        snap.source_fps, 23.976,
        "source_fps must survive event → snapshot"
    );
    assert_eq!(
        snap.nominal_fps, 30.0,
        "nominal_fps stays the OUTPUT nominal (grid)"
    );
    // (b) the lock rule reads source_fps (23.976) → LOCKED; nominal_fps (30) would DEGRADE.
    assert_eq!(
        snap.lock_state,
        sp_core::genlock::lock_state::LockState::Locked,
        "24-fps structural repeats must read LOCKED via source_fps, not grid nominal_fps"
    );
    assert_eq!(snap.lock_reason, "locked");
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

/// Minimal seeded snapshot for the readiness-count tests — every field zeroed
/// except the identity, so `reported_pipelines()` (a map-len read) can be
/// exercised without the full engine heartbeat path.
fn mk_reported_snapshot(playlist_id: i64) -> PipelineHealthSnapshot {
    PipelineHealthSnapshot {
        playlist_id,
        ndi_name: format!("SP-{playlist_id}"),
        state: PlaybackStateLabel::Idle,
        frames_submitted_total: 0,
        frames_submitted_last_5s: 0,
        observed_fps: 0.0,
        nominal_fps: 0.0,
        source_fps: 0.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: crate::playback::clock_health::ClockHealth::default(),
        pacing: Default::default(),
        audio: Default::default(),
        lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
        lock_reason: String::new(),
        transport: sp_core::playback::TransportState::Idle,
    }
}
