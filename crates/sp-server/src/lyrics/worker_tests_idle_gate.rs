//! Worker-loop tests for the #154 idle gate (no heavy processing while the
//! wall is in use). Included as a sibling of `worker.rs` so it can reach the
//! private `process_next` + the gate seam. The pure decision core is tested in
//! `idle_gate_tests.rs`; here we prove the loop-level behaviour: a busy wall
//! defers heavy work with no backoff penalty and surfaces a waiting state,
//! while an idle wall / a disabled gate proceed.

use super::*;
use crate::obs::ObsState;
use crate::playback::ndi_health::{NdiHealthRegistry, PipelineHealthSnapshot, PlaybackStateLabel};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Minimal `Playing` health snapshot for a given NDI output. Mirrors the
/// engine's real construction; the fields the gate reads are `state` (Playing)
/// and `ndi_name` (for the log detail).
fn playing_snapshot(playlist_id: i64, ndi_name: &str) -> PipelineHealthSnapshot {
    use crate::playback::ndi_health::{AudioStats, PacingStats};
    PipelineHealthSnapshot {
        playlist_id,
        ndi_name: ndi_name.to_string(),
        state: PlaybackStateLabel::Playing,
        connections: 1,
        frames_submitted_total: 0,
        frames_submitted_last_5s: 0,
        observed_fps: 24.0,
        nominal_fps: 24.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: crate::playback::clock_health::ClockHealth::default(),
        pacing: PacingStats::default(),
        audio: AudioStats::default(),
        lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
        lock_reason: String::new(),
        burn_on: false,
    }
}

fn registry_with(snapshots: Vec<PipelineHealthSnapshot>) -> Arc<NdiHealthRegistry> {
    let reg = Arc::new(NdiHealthRegistry::new());
    for s in snapshots {
        reg.update(s);
    }
    reg
}

async fn gate_worker(
    registry: Arc<NdiHealthRegistry>,
    obs: ObsState,
) -> (LyricsWorker, sqlx::SqlitePool) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let cache_dir = std::env::temp_dir().join(format!("sp_idle_gate_{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&cache_dir);
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = LyricsWorker::new_for_test(pool.clone(), cache_dir, events_tx)
        .with_wall_handles(registry, Arc::new(RwLock::new(obs)));
    (worker, pool)
}

/// The core #154 guarantee: with a Playing snapshot the worker loop defers the
/// heavy pick — the eligible song is left UNTOUCHED (not marked failed, no
/// backoff penalty) and the dashboard shows "waiting — wall in use".
#[tokio::test]
async fn process_next_defers_heavy_work_while_playing() {
    let registry = registry_with(vec![playing_snapshot(7, "SP-fast")]);
    let (worker, pool) = gate_worker(registry, ObsState::default()).await;

    // An eligible song sits in the queue (bucket 1: never processed).
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_pipeline_version, lyrics_manual_priority) \
         VALUES (1, 1, 'yt_busy', 1, 0, 0, 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    worker.process_next().await;

    // Heavy stage NOT spawned and the row is not marked failed: still bucket 1.
    let (has_lyrics, source, attempts): (i64, Option<String>, i64) = sqlx::query_as(
        "SELECT has_lyrics, lyrics_source, COALESCE(lyrics_attempts, 0) FROM videos WHERE id = 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(has_lyrics, 0, "song must not be marked processed");
    assert!(source.is_none(), "song must not be marked failed/no_source");
    assert_eq!(attempts, 0, "no backoff penalty — attempts must stay 0");

    // The worker surfaces WHY nothing is happening.
    let proc = worker.current_processing().read().await.clone();
    let proc = proc.expect("waiting state should be set");
    assert!(
        proc.stage.contains("waiting — wall in use"),
        "stage was: {}",
        proc.stage
    );
    assert!(
        proc.stage.contains("SP-fast"),
        "waiting detail should name the playing output; stage was: {}",
        proc.stage
    );
}

/// The operator override: `lyrics_gate_when_playing=false` disables the gate —
/// heavy work proceeds even while Playing (today's behaviour).
#[tokio::test]
async fn gate_off_setting_proceeds_even_while_playing() {
    let registry = registry_with(vec![playing_snapshot(7, "SP-fast")]);
    let (worker, pool) = gate_worker(registry, ObsState::default()).await;
    crate::db::models::set_setting(&pool, "lyrics_gate_when_playing", "false")
        .await
        .unwrap();

    let (defer, activity) = worker.wall_gate_should_defer().await;
    assert!(!defer, "gate OFF must not defer");
    assert!(
        activity.any_playing,
        "the wall is still detected as playing"
    );
}

/// OBS streaming (with nothing Playing) also gates heavy work — the defence the
/// owner asked for beyond the Playing signal.
#[tokio::test]
async fn gate_defers_when_obs_streaming() {
    let obs = ObsState {
        streaming: true,
        ..Default::default()
    };
    let (worker, _pool) = gate_worker(registry_with(vec![]), obs).await;

    let (defer, activity) = worker.wall_gate_should_defer().await;
    assert!(defer, "OBS streaming must defer heavy work");
    assert!(activity.obs_streaming);
    assert!(!activity.any_playing);
}

/// An idle wall (no output playing, OBS not streaming/recording) lets the
/// worker proceed.
#[tokio::test]
async fn gate_proceeds_when_wall_idle() {
    let (worker, _pool) = gate_worker(registry_with(vec![]), ObsState::default()).await;
    let (defer, activity) = worker.wall_gate_should_defer().await;
    assert!(!defer, "idle wall must proceed");
    assert!(!activity.in_use());
}
