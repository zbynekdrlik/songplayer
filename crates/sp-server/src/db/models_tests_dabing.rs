//! Unit tests for the dubbing D1 (#180) query surface, on an in-memory pool.
//! Covers the request toggle + its priority side effects, newest-first listing,
//! mixer-ratio clamp/persist, and the pure `dub_chain_state` derivation.

use super::*;
use crate::db::{create_memory_pool, run_migrations};

async fn setup() -> SqlitePool {
    let pool = create_memory_pool().await.unwrap();
    run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (7, 'Dabing', '')")
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// Insert a video into playlist 7, return its id.
async fn insert_video(pool: &SqlitePool, youtube_id: &str, title: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title) VALUES (7, ?, ?) RETURNING id",
    )
    .bind(youtube_id)
    .bind(title)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn col_i64(pool: &SqlitePool, id: i64, col: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT {col} FROM videos WHERE id = ?"))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn col_str(pool: &SqlitePool, id: i64, col: &str) -> String {
    sqlx::query_scalar(&format!("SELECT {col} FROM videos WHERE id = ?"))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn col_opt_str(pool: &SqlitePool, id: i64, col: &str) -> Option<String> {
    sqlx::query_scalar(&format!("SELECT {col} FROM videos WHERE id = ?"))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn set_dub_requested_true_raises_status_and_both_priority_flags() {
    let pool = setup().await;
    let id = insert_video(&pool, "v1", "Testimony").await;

    let affected = set_dub_requested(&pool, id, true).await.unwrap();
    assert_eq!(affected, 1);

    assert_eq!(col_i64(&pool, id, "dub_requested").await, 1);
    assert_eq!(col_str(&pool, id, "dub_status").await, "queued");
    assert_eq!(
        col_i64(&pool, id, "stem_manual_priority").await,
        1,
        "requesting a dub must raise the stems manual-priority bucket"
    );
    assert_eq!(
        col_i64(&pool, id, "lyrics_manual_priority").await,
        1,
        "requesting a dub must raise the lyrics manual-priority bucket"
    );
    assert!(
        col_opt_str(&pool, id, "dub_requested_at").await.is_some(),
        "requesting a dub must stamp dub_requested_at"
    );
}

#[tokio::test]
async fn set_dub_requested_false_clears_request_and_status() {
    let pool = setup().await;
    let id = insert_video(&pool, "v1", "Testimony").await;
    set_dub_requested(&pool, id, true).await.unwrap();

    let affected = set_dub_requested(&pool, id, false).await.unwrap();
    assert_eq!(affected, 1);

    assert_eq!(col_i64(&pool, id, "dub_requested").await, 0);
    assert_eq!(col_str(&pool, id, "dub_status").await, "none");
    // Un-requesting leaves the manual-priority flags — the video may still want
    // its stems / lyrics processed.
    assert_eq!(col_i64(&pool, id, "stem_manual_priority").await, 1);
}

#[tokio::test]
async fn set_dub_requested_missing_row_affects_nothing() {
    let pool = setup().await;
    let affected = set_dub_requested(&pool, 9999, true).await.unwrap();
    assert_eq!(affected, 0);
}

#[tokio::test]
async fn list_dub_videos_returns_only_requested_newest_first() {
    let pool = setup().await;
    let a = insert_video(&pool, "va", "A").await;
    let b = insert_video(&pool, "vb", "B").await;
    let c = insert_video(&pool, "vc", "C (not requested)").await;

    // Request a, then b — b is newer. Set explicit timestamps so the ordering
    // is deterministic regardless of same-millisecond inserts.
    set_dub_requested(&pool, a, true).await.unwrap();
    sqlx::query("UPDATE videos SET dub_requested_at = '2026-09-18T10:00:00.000Z' WHERE id = ?")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    set_dub_requested(&pool, b, true).await.unwrap();
    sqlx::query("UPDATE videos SET dub_requested_at = '2026-09-18T10:05:00.000Z' WHERE id = ?")
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();

    let rows = list_dub_videos(&pool).await.unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r.video_id).collect();
    assert_eq!(
        ids,
        vec![b, a],
        "newest request first, unrequested excluded"
    );
    assert!(!ids.contains(&c));
    assert_eq!(rows[0].title, "B");
    assert_eq!(rows[0].dub_status, "queued");
    assert_eq!(rows[0].chain_state, "queued");
}

#[tokio::test]
async fn list_dub_videos_row_carries_title_song_fallback_and_lyrics_flag() {
    let pool = setup().await;
    let id = insert_video(&pool, "vx", "raw-title").await;
    sqlx::query("UPDATE videos SET song = 'Nice Song', has_lyrics = 1 WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    set_dub_requested(&pool, id, true).await.unwrap();

    let rows = list_dub_videos(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "Nice Song", "song wins over title");
    assert!(rows[0].lyrics_present);
    // queued + lyrics present ⇒ chain refines to transcript.
    assert_eq!(rows[0].chain_state, "transcript");
}

#[tokio::test]
async fn videos_payload_reflects_dub_requested_flag() {
    // Exercises `row_to_video`'s `dub_requested != 0` mapping: a requested video
    // reads back true, an untouched one false. Kills the `!= 0` → `== 0` mutant.
    let pool = setup().await;
    let requested = insert_video(&pool, "vreq", "Requested").await;
    let plain = insert_video(&pool, "vplain", "Plain").await;
    set_dub_requested(&pool, requested, true).await.unwrap();

    let videos = crate::db::models::get_videos_for_playlist(&pool, 7)
        .await
        .unwrap();
    let req = videos.iter().find(|v| v.id == requested).unwrap();
    let pl = videos.iter().find(|v| v.id == plain).unwrap();
    assert!(
        req.dub_requested,
        "requested video must read dub_requested=true"
    );
    assert!(
        !pl.dub_requested,
        "untouched video must read dub_requested=false"
    );
    assert_eq!(req.dub_status.as_deref(), Some("queued"));
    assert_eq!(pl.dub_status.as_deref(), Some("none"));
}

#[tokio::test]
async fn set_dub_mix_ratio_clamps_and_persists() {
    let pool = setup().await;
    let id = insert_video(&pool, "vr", "Ratio").await;

    assert_eq!(set_dub_mix_ratio(&pool, id, 0.5).await.unwrap(), (0.5, 1));
    let stored: f64 = sqlx::query_scalar("SELECT dub_mix_ratio FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0.5);

    // Over-range clamps.
    assert_eq!(set_dub_mix_ratio(&pool, id, 1.7).await.unwrap(), (1.0, 1));
    assert_eq!(set_dub_mix_ratio(&pool, id, -0.3).await.unwrap(), (0.0, 1));
    let stored: f64 = sqlx::query_scalar("SELECT dub_mix_ratio FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0.0);

    // NaN falls back to the dub-only default.
    assert_eq!(
        set_dub_mix_ratio(&pool, id, f64::NAN).await.unwrap(),
        (1.0, 1)
    );

    // A missing row reports 0 rows affected (so the handler can 404).
    let (v, affected) = set_dub_mix_ratio(&pool, 9999, 0.5).await.unwrap();
    assert_eq!((v, affected), (0.5, 0));
}

#[test]
fn dub_chain_state_maps_explicit_statuses() {
    assert_eq!(
        dub_chain_state("failed", Some("done"), true),
        DubChainState::Failed
    );
    assert_eq!(dub_chain_state("ready", None, false), DubChainState::Ready);
    assert_eq!(dub_chain_state("synth", None, false), DubChainState::Synth);
    assert_eq!(
        dub_chain_state("translation", None, false),
        DubChainState::Translation
    );
    assert_eq!(
        dub_chain_state("transcript", None, false),
        DubChainState::Transcript
    );
    assert_eq!(dub_chain_state("stems", None, false), DubChainState::Stems);
}

#[test]
fn dub_chain_state_refines_queued_by_available_artefacts() {
    // Bare queued with nothing done.
    assert_eq!(
        dub_chain_state("queued", None, false),
        DubChainState::Queued
    );
    assert_eq!(dub_chain_state("none", None, false), DubChainState::Queued);
    // Stems already separated ⇒ show Stems.
    assert_eq!(
        dub_chain_state("queued", Some("done"), false),
        DubChainState::Stems
    );
    // Lyrics already present ⇒ transcript is available, beats stems.
    assert_eq!(
        dub_chain_state("queued", Some("done"), true),
        DubChainState::Transcript
    );
    assert_eq!(
        dub_chain_state("queued", None, true),
        DubChainState::Transcript
    );
    // A failed status is never overridden by available artefacts.
    assert_eq!(
        dub_chain_state("failed", Some("done"), true),
        DubChainState::Failed
    );
}

#[test]
fn dub_chain_state_wire_strings_are_stable() {
    assert_eq!(DubChainState::Queued.as_str(), "queued");
    assert_eq!(DubChainState::Stems.as_str(), "stems");
    assert_eq!(DubChainState::Transcript.as_str(), "transcript");
    assert_eq!(DubChainState::Translation.as_str(), "translation");
    assert_eq!(DubChainState::Synth.as_str(), "synth");
    assert_eq!(DubChainState::Ready.as_str(), "ready");
    assert_eq!(DubChainState::Failed.as_str(), "failed");
}

// ── D4 (#183): dub synthesis chain selectors ────────────────────────────────────

/// Make `id` a downloaded, dub-requested job at `status` with a distinct
/// `dub_requested_at` (later `req_at` = higher priority) and normalized audio.
async fn make_dub_job(pool: &SqlitePool, id: i64, status: &str, req_at: &str) {
    sqlx::query(
        "UPDATE videos SET dub_requested = 1, normalized = 1, \
             audio_file_path = '/c/a_audio.flac', dub_status = ?, dub_requested_at = ? \
         WHERE id = ?",
    )
    .bind(status)
    .bind(req_at)
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

#[test]
fn dub_stems_ready_needs_both_non_empty_paths() {
    assert!(dub_stems_ready(Some("/v.flac"), Some("/i.flac")));
    assert!(!dub_stems_ready(None, Some("/i.flac")));
    assert!(!dub_stems_ready(Some("/v.flac"), None));
    assert!(!dub_stems_ready(Some(""), Some("/i.flac")));
    assert!(!dub_stems_ready(None, None));
}

#[test]
fn dub_stems_state_collapses_presence_and_status() {
    // Both files present → Ready (regardless of a stale status).
    assert_eq!(
        dub_stems_state(Some("/v.flac"), Some("/i.flac"), Some("unsupported")),
        DubStemsState::Ready
    );
    assert_eq!(
        dub_stems_state(Some("/v.flac"), Some("/i.flac"), None),
        DubStemsState::Ready
    );
    // Absent + unsupported → Unsupported (terminal; will never arrive).
    assert_eq!(
        dub_stems_state(None, None, Some("unsupported")),
        DubStemsState::Unsupported
    );
    // Absent + any other status → Pending (a separation may still run).
    assert_eq!(dub_stems_state(None, None, None), DubStemsState::Pending);
    assert_eq!(
        dub_stems_state(None, None, Some("failed")),
        DubStemsState::Pending
    );
    // A half-pair is not Ready.
    assert_eq!(
        dub_stems_state(Some("/v.flac"), None, None),
        DubStemsState::Pending
    );
}

#[test]
fn synth_ready_never_waits_for_stems() {
    use SynthDecision::*;
    // Not downloaded → wait for download (the ONLY wait state).
    assert_eq!(
        synth_ready(false, DubStemsState::Ready, Some(60_000)),
        WaitForDownload
    );
    // Stems ready → proceed straight to synth.
    assert_eq!(
        synth_ready(true, DubStemsState::Ready, Some(60_000)),
        Proceed
    );
    // Stems unsupported (over the 15-min cap) → proceed WITHOUT raising priority
    // (this is the round-2 fix: the 40-min sample no longer stalls at `stems`).
    assert_eq!(
        synth_ready(true, DubStemsState::Unsupported, Some(2_400_000)),
        Proceed
    );
    // Stems pending AND within the 15-min cap → proceed AND raise priority so a
    // later separation enriches the mix.
    assert_eq!(
        synth_ready(true, DubStemsState::Pending, Some(60_000)),
        ProceedRaisePriority
    );
    // Unknown duration is "supported" → still raise priority.
    assert_eq!(
        synth_ready(true, DubStemsState::Pending, None),
        ProceedRaisePriority
    );
    // Stems pending but BEYOND the cap (separation would only be unsupported) →
    // proceed WITHOUT raising priority (raising it would be pointless).
    assert_eq!(
        synth_ready(true, DubStemsState::Pending, Some(2_400_000)),
        Proceed
    );
    // Boundary: exactly 15 min is supported (raise); one ms over is not (proceed).
    assert_eq!(
        synth_ready(true, DubStemsState::Pending, Some(900_000)),
        ProceedRaisePriority
    );
    assert_eq!(
        synth_ready(true, DubStemsState::Pending, Some(900_001)),
        Proceed
    );
}

#[tokio::test]
async fn get_next_dub_job_picks_newest_requested_first() {
    let pool = setup().await;
    let a = insert_video(&pool, "va", "A").await;
    let b = insert_video(&pool, "vb", "B").await;
    make_dub_job(&pool, a, "queued", "2026-09-18T10:00:00.000Z").await;
    make_dub_job(&pool, b, "queued", "2026-09-18T11:00:00.000Z").await; // newer

    let job = get_next_dub_job(&pool).await.unwrap().expect("a job");
    assert_eq!(
        job.video_id, b,
        "newest dub_requested_at wins (priority queue)"
    );
    assert_eq!(job.dub_status, "queued");
}

#[tokio::test]
async fn get_next_dub_job_skips_none_ready_and_undownloaded() {
    let pool = setup().await;
    let ready = insert_video(&pool, "vr", "ready").await;
    let none = insert_video(&pool, "vn", "none").await;
    let undl = insert_video(&pool, "vu", "undownloaded").await;
    make_dub_job(&pool, ready, "ready", "2026-09-18T10:00:00.000Z").await;
    let _ = none; // default dub_status='none' + not requested → ineligible
    // requested but NOT normalized / no audio:
    sqlx::query("UPDATE videos SET dub_requested = 1, dub_status = 'queued' WHERE id = ?")
        .bind(undl)
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        get_next_dub_job(&pool).await.unwrap().is_none(),
        "ready/none/undownloaded rows are not eligible"
    );
}

#[tokio::test]
async fn get_next_dub_job_respects_backoff() {
    let pool = setup().await;
    let id = insert_video(&pool, "vf", "failed").await;
    make_dub_job(&pool, id, "failed", "2026-09-18T10:00:00.000Z").await;
    sqlx::query(
        "UPDATE videos SET dub_next_attempt_at = \
             strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+3600 seconds') WHERE id = ?",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(get_next_dub_job(&pool).await.unwrap().is_none());

    sqlx::query(
        "UPDATE videos SET dub_next_attempt_at = \
             strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 seconds') WHERE id = ?",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(get_next_dub_job(&pool).await.unwrap().unwrap().video_id, id);
}

#[tokio::test]
async fn mark_dub_transitions_advance_status() {
    let pool = setup().await;
    let id = insert_video(&pool, "vt", "T").await;
    make_dub_job(&pool, id, "queued", "2026-09-18T10:00:00.000Z").await;

    // #183 round 2: raising the stems priority does NOT park dub_status at
    // `stems` — it only raises stem_manual_priority; the dub proceeds to synth.
    raise_dub_stem_priority(&pool, id).await.unwrap();
    assert_eq!(
        col_str(&pool, id, "dub_status").await,
        "queued",
        "raising stem priority must NOT change dub_status"
    );
    assert_eq!(col_i64(&pool, id, "stem_manual_priority").await, 1);

    mark_dub_synth(&pool, id).await.unwrap();
    assert_eq!(col_str(&pool, id, "dub_status").await, "synth");

    mark_dub_ready(&pool, id, "/c/a_dub.flac", "gemini-live-translate")
        .await
        .unwrap();
    assert_eq!(col_str(&pool, id, "dub_status").await, "ready");
    assert_eq!(
        col_opt_str(&pool, id, "dub_file_path").await,
        Some("/c/a_dub.flac".to_string())
    );
    assert_eq!(
        col_opt_str(&pool, id, "dub_engine").await,
        Some("gemini-live-translate".to_string())
    );
    assert_eq!(col_i64(&pool, id, "dub_attempts").await, 0);
}

#[tokio::test]
async fn dub_ratio_if_ready_only_for_videos_with_a_dub_file() {
    let pool = setup().await;
    let ready = insert_video(&pool, "vready", "ready").await;
    let nodub = insert_video(&pool, "vnodub", "no dub").await;
    sqlx::query(
        "UPDATE videos SET dub_file_path = '/c/a_dub.flac', dub_mix_ratio = 0.3 WHERE id = ?",
    )
    .bind(ready)
    .execute(&pool)
    .await
    .unwrap();
    // A video with a finished dub → Some(stored ratio).
    assert_eq!(dub_ratio_if_ready(&pool, ready).await.unwrap(), Some(0.3));
    // A video without a dub file → None (play-start seeding is a no-op).
    assert_eq!(dub_ratio_if_ready(&pool, nodub).await.unwrap(), None);
    // A missing id → None.
    assert_eq!(dub_ratio_if_ready(&pool, 99999).await.unwrap(), None);
}

#[tokio::test]
async fn record_dub_deferral_increments_and_marks_failed() {
    let pool = setup().await;
    let id = insert_video(&pool, "vd", "D").await;
    make_dub_job(&pool, id, "synth", "2026-09-18T10:00:00.000Z").await;

    let n1 = record_dub_deferral(&pool, id, "boom", std::time::Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(n1, 1);
    assert_eq!(col_str(&pool, id, "dub_status").await, "failed");
    assert_eq!(
        col_opt_str(&pool, id, "dub_error").await,
        Some("boom".to_string())
    );
    let n2 = record_dub_deferral(&pool, id, "boom2", std::time::Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(n2, 2);
}
