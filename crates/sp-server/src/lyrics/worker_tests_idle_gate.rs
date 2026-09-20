//! Worker-loop tests for the #154 idle gate (no heavy processing while the
//! wall is in use). Included as a sibling of `worker.rs` so it can reach the
//! private `process_next` + the gate seam. The pure decision core is tested in
//! `idle_gate_tests.rs`; here we prove the loop-level behaviour: a busy wall
//! defers heavy work with no backoff penalty and surfaces a waiting state,
//! while an idle wall / a disabled gate proceed.

use super::*;
use crate::lyrics::idle_gate::{GateLog, IdleSettle, WALL_IDLE_SETTLE, WallActivity};
use crate::obs::ObsState;
use crate::playback::ndi_health::{NdiHealthRegistry, PipelineHealthSnapshot, PlaybackStateLabel};
use std::sync::Arc;
use std::time::{Duration, Instant};
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
        recovery_step: None,
        sender_url: None,
    }
}

fn registry_with(snapshots: Vec<PipelineHealthSnapshot>) -> Arc<NdiHealthRegistry> {
    let reg = Arc::new(NdiHealthRegistry::new());
    for s in snapshots {
        // Mirror production: `ensure_pipeline` registers a created pipeline (#167),
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

/// The core #154 guarantee, now scoped to IDLE-ONLY mode (#162): with
/// `lyrics_processing_mode=idle-only` and a Playing snapshot the worker loop
/// defers the heavy pick — the eligible song is left UNTOUCHED (not marked
/// failed, no backoff penalty) and the dashboard shows "waiting — wall in use".
/// In the default low-priority mode the loop does NOT defer (proven by
/// `low_priority_mode_does_not_defer_while_playing`).
#[tokio::test]
async fn idle_only_mode_defers_heavy_work_while_playing() {
    let registry = registry_with(vec![playing_snapshot(7, "SP-fast")]);
    let (worker, pool) = gate_worker(registry, ObsState::default()).await;
    crate::db::models::set_setting(&pool, "lyrics_processing_mode", "idle-only")
        .await
        .unwrap();

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

/// #162 core: in `low-priority` (the DEFAULT) the loop-level gate NEVER defers,
/// even while the wall is Playing — the queue drains continuously at reduced
/// priority instead of stopping. `idle-only` still defers (below).
#[tokio::test]
async fn low_priority_mode_does_not_defer_while_playing() {
    let registry = registry_with(vec![playing_snapshot(7, "SP-fast")]);
    let (worker, _pool) = gate_worker(registry, ObsState::default()).await;

    let (defer, activity) = worker
        .loop_should_defer(crate::lyrics::heavy_plan::ProcessingMode::LowPriority)
        .await;
    assert!(!defer, "low-priority must not defer even while playing");
    assert!(
        activity.any_playing,
        "the wall is still detected as playing"
    );
}

/// #162: `idle-only` mode still defers the loop-level pick while the wall plays.
#[tokio::test]
async fn idle_only_mode_defers_at_loop_level_while_playing() {
    let registry = registry_with(vec![playing_snapshot(7, "SP-fast")]);
    let (worker, _pool) = gate_worker(registry, ObsState::default()).await;

    let (defer, activity) = worker
        .loop_should_defer(crate::lyrics::heavy_plan::ProcessingMode::IdleOnly)
        .await;
    assert!(defer, "idle-only must defer while playing");
    assert!(activity.any_playing);
}

/// #162: `processing_mode` reads the `lyrics_processing_mode` setting, defaulting
/// to low-priority when unset and parsing idle-only when the operator sets it.
#[tokio::test]
async fn processing_mode_reads_setting_with_low_priority_default() {
    use crate::lyrics::heavy_plan::ProcessingMode;
    let (worker, pool) = gate_worker(registry_with(vec![]), ObsState::default()).await;

    assert_eq!(
        worker.processing_mode().await,
        ProcessingMode::LowPriority,
        "unset setting defaults to low-priority"
    );

    crate::db::models::set_setting(&pool, "lyrics_processing_mode", "idle-only")
        .await
        .unwrap();
    assert_eq!(
        worker.processing_mode().await,
        ProcessingMode::IdleOnly,
        "operator can select idle-only"
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

/// After the idle-settle fix (2026-09-14 incident) a FRESHLY idle wall no longer
/// resumes on the first sample: it must first read idle continuously for
/// `WALL_IDLE_SETTLE`. The elapsed-settle → proceed path is proven by the pure
/// `idle_settle_*` tests (they inject time); the worker seam reads the real
/// clock, so here we only assert the freshly-idle sample still defers.
#[tokio::test]
async fn gate_freshly_idle_wall_defers_until_settled() {
    // A KNOWN-idle wall: a created + reported pipeline in the Idle state (not an
    // empty registry, which is #167-UNKNOWN and would read as in-use). The point
    // of this test is the idle-settle hysteresis on a genuinely-idle wall.
    let mut idle = playing_snapshot(7, "SP-fast");
    idle.state = PlaybackStateLabel::Idle;
    let (worker, _pool) = gate_worker(registry_with(vec![idle]), ObsState::default()).await;
    let (defer, activity) = worker.wall_gate_should_defer().await;
    assert!(
        defer,
        "a freshly-idle wall must defer during the settle window"
    );
    assert!(!activity.in_use());
    let detail = worker.wall_busy_detail(activity).await;
    assert!(
        detail.contains("settling"),
        "idle-but-settling detail should mention settling; was: {detail}"
    );
}

// ---- IdleSettle pure hysteresis ------------------------------------------

/// The core guarantee: a single idle sample is not enough — heavy work resumes
/// only after `WALL_IDLE_SETTLE` of CONTINUOUS idle.
#[test]
fn idle_settle_defers_until_thirty_seconds_of_continuous_idle() {
    let mut s = IdleSettle::default();
    let t0 = Instant::now();
    assert!(s.defer(true, t0), "in use → defer");
    assert!(s.defer(false, t0), "just went idle → still defer");
    assert!(
        s.defer(false, t0 + Duration::from_secs(29)),
        "29s idle → still defer"
    );
    assert!(
        !s.defer(false, t0 + Duration::from_secs(30)),
        "30s continuous idle → resume"
    );
    assert_eq!(
        s.idle_for(t0 + Duration::from_secs(30)),
        Some(Duration::from_secs(30))
    );
    assert_eq!(WALL_IDLE_SETTLE, Duration::from_secs(30));
}

/// A busy sample mid-settle resets the clock — the wall must read idle for a
/// FRESH full window after any interruption.
#[test]
fn idle_settle_busy_sample_resets_the_clock() {
    let mut s = IdleSettle::default();
    let t0 = Instant::now();
    assert!(s.defer(false, t0), "idle t0 → defer (settling)");
    assert!(
        s.defer(false, t0 + Duration::from_secs(20)),
        "20s idle → defer"
    );
    assert!(s.defer(true, t0 + Duration::from_secs(25)), "busy → defer");
    assert_eq!(
        s.idle_for(t0 + Duration::from_secs(25)),
        None,
        "busy → idle_for None"
    );
    assert!(
        s.defer(false, t0 + Duration::from_secs(26)),
        "idle again → settle restarts"
    );
    assert!(
        s.defer(false, t0 + Duration::from_secs(55)),
        "29s since restart → still defer"
    );
    assert!(
        !s.defer(false, t0 + Duration::from_secs(56)),
        "30s since restart → resume"
    );
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

/// A mid-job abort surfaces the same song-less "waiting — wall in use" badge the
/// pre-flight gate uses, carrying the cause detail — so the dashboard keeps a
/// stable waiting state through the abort.
#[tokio::test]
async fn enter_wall_abort_sets_waiting_badge() {
    let (worker, _pool) = gate_worker(registry_with(vec![]), ObsState::default()).await;
    worker.enter_wall_abort("output playing").await;
    let proc = worker.current_processing().read().await.clone();
    let proc = proc.expect("waiting badge should be set");
    assert!(
        proc.stage.contains("waiting — wall in use"),
        "stage was: {}",
        proc.stage
    );
    assert!(
        proc.stage.contains("output playing"),
        "waiting detail should name the cause; stage was: {}",
        proc.stage
    );
}

/// The gate seam both workers share: a disabled gate never defers, regardless of
/// the settle clock (unchanged behaviour).
#[test]
fn gate_log_defer_settled_never_defers_when_gate_disabled() {
    let mut log = GateLog::default();
    let in_use = WallActivity {
        any_playing: true,
        obs_streaming: false,
        obs_recording: false,
        known: true,
    };
    assert!(
        !log.defer_settled(false, in_use, Instant::now()),
        "gate OFF must never defer even while busy"
    );
}

/// Regression (2026-09-14, LyricsWorker level): the wall goes idle for ONE
/// sample right after Playing — the pre-fix code resumed a heavy job in exactly
/// this gap. The settle window must keep deferring. This assertion returns
/// `false` (proceeds) on the pre-fix code and `true` after the fix.
#[tokio::test]
async fn single_idle_sample_after_busy_still_defers() {
    let registry = registry_with(vec![playing_snapshot(7, "SP-fast")]);
    let (worker, _pool) = gate_worker(Arc::clone(&registry), ObsState::default()).await;

    // Busy wall defers, and primes the settle clock as "in use".
    let (defer, activity) = worker.wall_gate_should_defer().await;
    assert!(defer, "playing wall must defer");
    assert!(activity.any_playing);

    // Flip the same output to Idle — a single idle sample.
    let mut idle = playing_snapshot(7, "SP-fast");
    idle.state = PlaybackStateLabel::Idle;
    registry.update(idle);

    let (defer, activity) = worker.wall_gate_should_defer().await;
    assert!(
        defer,
        "a single idle sample right after busy must STILL defer (settle not elapsed)"
    );
    assert!(!activity.in_use(), "the wall reads idle now");

    let detail = worker.wall_busy_detail(activity).await;
    assert!(
        detail.contains("settling"),
        "idle-but-settling detail should mention settling; was: {detail}"
    );
}
