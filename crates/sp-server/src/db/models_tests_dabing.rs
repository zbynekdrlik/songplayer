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
