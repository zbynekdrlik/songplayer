//! Worker-loop tests for the wall-activity seam (#162: heavy work never waits
//! for an idle wall, it runs CPU-idle while the wall is in use). Included as a
//! sibling of `worker.rs` so it can reach the private `process_next` + the
//! seam. The pure decision core is tested in `idle_gate_tests.rs`.

use super::*;
use crate::obs::ObsState;
use crate::playback::ndi_health::{NdiHealthRegistry, PipelineHealthSnapshot, PlaybackStateLabel};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Minimal `Playing` health snapshot for a given `ndi_name` label. Mirrors the
/// engine's real construction; the fields the gate reads are `state` (Playing)
/// and `ndi_name` (for the log detail).
fn playing_snapshot(playlist_id: i64, ndi_name: &str) -> PipelineHealthSnapshot {
    use crate::playback::ndi_health::{AudioStats, PacingStats};
    PipelineHealthSnapshot {
        playlist_id,
        ndi_name: ndi_name.to_string(),
        state: PlaybackStateLabel::Playing,
        frames_submitted_total: 0,
        frames_submitted_last_5s: 0,
        observed_fps: 24.0,
        nominal_fps: 24.0,
        source_fps: 24.0,
        last_submit_ts: None,
        last_heartbeat_ts: None,
        consecutive_bad_polls: 0,
        degraded_reason: None,
        clock: crate::playback::clock_health::ClockHealth::default(),
        pacing: PacingStats::default(),
        audio: AudioStats::default(),
        lock_state: sp_core::genlock::lock_state::LockState::Unlocked,
        lock_reason: String::new(),
        transport: sp_core::playback::TransportState::Idle,
        open_failures: None,
        gpu_decoded: false,
    }
}

fn registry_with(snapshots: Vec<PipelineHealthSnapshot>) -> Arc<NdiHealthRegistry> {
    let reg = Arc::new(NdiHealthRegistry::new());
    for s in snapshots {
        // Mirror production: `ensure_pipeline_inner` registers a created pipeline (#167),
        // then heartbeats report it. Registering here so `created == reported`
        // makes the wall reading KNOWN (activity_known), the state these gate
        // tests assume — an empty registry stays UNKNOWN (created 0), which is the
        // #167 startup default.
        reg.register_pipeline();
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
    // The eligible-song fixtures reference playlist 1 — seed it, the memory
    // pool enforces the videos.playlist_id foreign key (CI: "FOREIGN KEY
    // constraint failed" on the first insert).
    sqlx::query(
        "INSERT OR IGNORE INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'gate_pl', 'u', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let cache_dir = std::env::temp_dir().join(format!("sp_idle_gate_{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&cache_dir);
    let (events_tx, _rx) = tokio::sync::broadcast::channel::<sp_core::ws::ServerMsg>(16);
    let worker = LyricsWorker::new_for_test(pool.clone(), cache_dir, events_tx)
        .with_wall_handles(registry, Arc::new(RwLock::new(obs)));
    (worker, pool)
}

/// #286: the idle-only mode is gone. A box that still stores
/// `lyrics_processing_mode = idle-only` runs exactly like every other: the
/// loop never parks on a playing wall (no "waiting — wall in use" badge) and a
/// heavy step takes the CPU-idle plan while the wall plays.
#[tokio::test]
async fn a_stored_idle_only_setting_runs_low_priority() {
    // Calls `process_next`, which reads the process-global #184 G0.1 dub flag at
    // the top; serialize + clear it so a parallel flag test can't make this tick
    // defer for a (phantom) dub instead.
    let _lk = crate::lyrics::heavy_slot::DUB_FLAG_SERIAL.lock().await;
    crate::lyrics::heavy_slot::set_dub_slot_wanted(false);
    let registry = registry_with(vec![playing_snapshot(7, "SP-fast")]);
    let (worker, pool) = gate_worker(registry, ObsState::default()).await;
    crate::db::models::set_setting(&pool, "lyrics_processing_mode", "idle-only")
        .await
        .unwrap();

    worker.process_next().await;
    assert!(
        worker.current_processing().read().await.is_none(),
        "the loop must not park on a playing wall"
    );

    let activity = worker.wall_activity().await;
    assert!(activity.any_playing, "the wall is detected as playing");
    let plan = crate::lyrics::heavy_plan::HeavyStepPlan::for_activity(activity);
    assert_eq!(plan, crate::lyrics::heavy_plan::HeavyStepPlan::cpu_idle());
    assert_eq!(
        LyricsWorker::stage_regime_suffix(activity),
        " (cpu, wall in use)"
    );
}

/// OBS streaming (with nothing Playing) reads as a wall in use — the defence
/// the owner asked for beyond the Playing signal.
#[tokio::test]
async fn obs_streaming_reads_as_wall_in_use() {
    let obs = ObsState {
        streaming: true,
        ..Default::default()
    };
    let (worker, _pool) = gate_worker(registry_with(vec![]), obs).await;

    let activity = worker.wall_activity().await;
    assert!(activity.in_use(), "OBS streaming must read as in use");
    assert!(activity.obs_streaming);
    assert!(!activity.any_playing);
}

// ---- #161 mid-job wall-abort (worker seam) --------------------------------

/// The core #161 guarantee at the worker level: with a Playing snapshot, a heavy
/// step run under `wall_abort` is KILLED (its future dropped, never run to
/// completion) within the ~2 s debounce, and the caller gets `Err(WallAbort)`.
/// Real clock on purpose: the fixture opens the sqlite pool inside the test, and
/// under `start_paused` sqlx's acquire timeout auto-advances (PoolTimedOut,
/// CI run 34926435178). The 1 s poll aborts the mock 30 s step after ~2 s.
#[tokio::test]
async fn wall_abort_kills_running_step_when_wall_playing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let registry = registry_with(vec![playing_snapshot(7, "SP-fast")]);
    let (worker, _pool) = gate_worker(registry, ObsState::default()).await;

    let completed = Arc::new(AtomicBool::new(false));
    let c = completed.clone();
    let heavy = async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        c.store(true, Ordering::SeqCst);
        7u32
    };

    // Gate ON, wall Playing → the running step is killed within ~2 s.
    let result = worker.wall_abort(heavy, true).await;
    assert!(
        result.is_err(),
        "a playing wall must abort the running heavy step"
    );
    assert!(
        !completed.load(Ordering::SeqCst),
        "aborted step must be dropped, never run to completion"
    );
    if let Err(e) = &result {
        assert!(
            e.detail.contains("playing"),
            "abort detail names the cause: {}",
            e.detail
        );
    }
}
