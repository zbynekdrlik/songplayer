//! Axum integration tests (#184 round G) for the ONE mixer console: the fader
//! PATCH + persist, the now-playing stems payload (#177, moved from karaoke.rs),
//! the videos-list `stems_state` marker, the stem re-enqueue endpoint, and the
//! 404 of the deleted karaoke / dub-mix routes. Included from `api/mix.rs` via
//! `#[path]`. Reuses the shared `routes::tests` AppState + router harness.

#![allow(unused_imports)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::api::routes::tests::{app, test_state};

/// The process-global `MixControl` is shared across parallel tests, so the two
/// fader tests serialize on this lock to keep their reads/writes deterministic.
/// An async-aware mutex: the guard is held across the handlers' `.await`s
/// (`clippy::await_holding_lock` rejects a `std::sync::MutexGuard` there).
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn insert_playlist(pool: &sqlx::SqlitePool, id: i64) {
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (?, 'p', 'u', 1)")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// Insert a normalized, stem-eligible video and return its id.
async fn insert_video(pool: &sqlx::SqlitePool, playlist_id: i64, youtube_id: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, normalized, file_path, audio_file_path) \
         VALUES (?, ?, ?, 1, ?, ?) RETURNING id",
    )
    .bind(playlist_id)
    .bind(youtube_id)
    .bind(format!("Song {youtube_id}"))
    .bind(format!("/c/{youtube_id}_video.mp4"))
    .bind(format!("/c/{youtube_id}_audio.flac"))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn get_json(app: axum::Router, uri: &str) -> serde_json::Value {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn patch_mix(app: axum::Router, body: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/mix")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn get_setting(pool: &sqlx::SqlitePool, key: &str) -> Option<String> {
    crate::db::models::get_setting(pool, key).await.unwrap()
}

/// Read a persisted mixer setting as f32 (panics if absent/unparseable — the tests
/// only call it on keys they just wrote).
async fn setting_f32(pool: &sqlx::SqlitePool, key: &str) -> f32 {
    get_setting(pool, key).await.unwrap().parse().unwrap()
}

/// Re-seed the process-global console to its default (song `(1,1,·)`, dub
/// `(0,1,1)`) so a kind-touching test starts from a known state regardless of what
/// a sibling left behind (round G2 — no active kind). Call under the `SERIAL` lock.
fn reset_console() {
    let _ = crate::stems::control::init(sp_core::mixer_model::MixConsole::default());
}

/// `GET /api/v1/mix` carries BOTH kind memories as nested objects and NO global
/// active kind (#184 round G2).
#[tokio::test]
async fn get_mix_returns_both_kind_memories_and_no_active_kind() {
    let _g = SERIAL.lock().await;
    reset_console();
    let state = test_state().await;

    let json = get_json(app(state), "/api/v1/mix").await;
    // Round G2: no flat faders and no global "kind".
    assert!(json.get("kind").is_none(), "no global active kind (G2)");
    assert!(
        json.get("vokaly").is_none(),
        "faders live under song/dub now"
    );
    // The default console: song (1,1), dub (0,1,1).
    assert!((json["song"]["vokaly"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    assert!((json["song"]["podklad"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    assert!((json["dub"]["vokaly"].as_f64().unwrap() - 0.0).abs() < 1e-6);
    assert!((json["dub"]["podklad"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    assert!((json["dub"]["dabing"].as_f64().unwrap() - 1.0).abs() < 1e-6);

    reset_console();
}

/// A DUB PATCH edits ONLY the dub memory + persists ONLY the dub keys; the song
/// memory and its keys are untouched (#184 round G2).
#[tokio::test]
async fn patch_dub_edits_only_the_dub_memory_and_dub_keys() {
    let _g = SERIAL.lock().await;
    reset_console();
    let state = test_state().await;
    let pool = state.pool.clone();

    // Distinct values per fader so a swapped fader→key persist would be caught.
    let (status, json) = patch_mix(
        app(state.clone()),
        r#"{"kind":"dub","vokaly":0.3,"podklad":0.4,"dabing":0.5}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["kind"], "dub");
    assert!((json["vokaly"].as_f64().unwrap() - 0.3).abs() < 1e-6);
    assert!((json["podklad"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    assert!((json["dabing"].as_f64().unwrap() - 0.5).abs() < 1e-6);

    // GET: only the dub memory changed; the song memory is untouched.
    let g = get_json(app(state), "/api/v1/mix").await;
    assert!((g["dub"]["vokaly"].as_f64().unwrap() - 0.3).abs() < 1e-6);
    assert!((g["dub"]["podklad"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    assert!((g["dub"]["dabing"].as_f64().unwrap() - 0.5).abs() < 1e-6);
    assert!(
        (g["song"]["vokaly"].as_f64().unwrap() - 1.0).abs() < 1e-6,
        "a dub edit must not touch the song memory"
    );

    // All THREE dub keys are persisted with their OWN values (not just vokaly), so a
    // swapped fader→key persist is caught; the song keys are never written.
    let dv = setting_f32(&pool, sp_core::config::SETTING_MIX_DUB_VOKALY).await;
    let dp = setting_f32(&pool, sp_core::config::SETTING_MIX_DUB_PODKLAD).await;
    let dd = setting_f32(&pool, sp_core::config::SETTING_MIX_DUB_DABING).await;
    assert!((dv - 0.3).abs() < 1e-6);
    assert!((dp - 0.4).abs() < 1e-6);
    assert!((dd - 0.5).abs() < 1e-6);
    assert_eq!(
        get_setting(&pool, sp_core::config::SETTING_MIX_SONG_VOKALY).await,
        None,
        "a dub edit must not persist a song key"
    );

    reset_console();
}

/// A SONG PATCH edits ONLY the song memory + persists ONLY the song keys; the dub
/// memory and its keys are untouched (#184 round G2).
#[tokio::test]
async fn patch_song_edits_only_the_song_memory_and_song_keys() {
    let _g = SERIAL.lock().await;
    reset_console();
    let state = test_state().await;
    let pool = state.pool.clone();

    // Distinct vokaly/podklad so a swapped fader→key persist would be caught.
    let (status, json) = patch_mix(
        app(state.clone()),
        r#"{"kind":"song","vokaly":0.3,"podklad":0.4}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["kind"], "song");
    assert!((json["vokaly"].as_f64().unwrap() - 0.3).abs() < 1e-6);
    assert!((json["podklad"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    // A song echo omits the unused dabing.
    assert!(json.get("dabing").is_none(), "song memory has no dabing");

    let g = get_json(app(state), "/api/v1/mix").await;
    assert!((g["song"]["vokaly"].as_f64().unwrap() - 0.3).abs() < 1e-6);
    assert!((g["song"]["podklad"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    assert!(
        (g["dub"]["vokaly"].as_f64().unwrap() - 0.0).abs() < 1e-6,
        "a song edit must not touch the dub memory"
    );

    // Both song keys are persisted with their own values; no dub key is written.
    assert!(
        (setting_f32(&pool, sp_core::config::SETTING_MIX_SONG_VOKALY).await - 0.3).abs() < 1e-6
    );
    assert!(
        (setting_f32(&pool, sp_core::config::SETTING_MIX_SONG_PODKLAD).await - 0.4).abs() < 1e-6
    );
    assert_eq!(
        get_setting(&pool, sp_core::config::SETTING_MIX_DUB_VOKALY).await,
        None,
        "a song edit must not persist a dub key"
    );

    reset_console();
}

/// A DUB PATCH with only one fader keeps the memory's other faders (#184 round G2).
#[tokio::test]
async fn patch_dub_partial_keeps_the_unspecified_faders() {
    let _g = SERIAL.lock().await;
    reset_console();
    let state = test_state().await;

    // Seed the dub memory, then PATCH ONLY vokaly — podklad/dabing must survive.
    let (s1, _) = patch_mix(
        app(state.clone()),
        r#"{"kind":"dub","vokaly":1.0,"podklad":0.4,"dabing":0.2}"#,
    )
    .await;
    assert_eq!(s1, StatusCode::OK);

    let (s2, json) = patch_mix(app(state), r#"{"kind":"dub","vokaly":0.0}"#).await;
    assert_eq!(s2, StatusCode::OK);
    assert!((json["vokaly"].as_f64().unwrap() - 0.0).abs() < 1e-6);
    assert!(
        (json["podklad"].as_f64().unwrap() - 0.4).abs() < 1e-6,
        "an omitted podklad keeps its current value"
    );
    assert!((json["dabing"].as_f64().unwrap() - 0.2).abs() < 1e-6);

    reset_console();
}

/// `PATCH /api/v1/mix` WITHOUT a `kind` is a 400 — the strip must name the memory it
/// edits (#184 round G2).
#[tokio::test]
async fn patch_without_kind_is_400() {
    let _g = SERIAL.lock().await;
    reset_console();
    let state = test_state().await;
    let (status, _) = patch_mix(app(state), r#"{"vokaly":0.3}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    reset_console();
}

/// `dabing` is not a song fader — a `kind:"song"` PATCH carrying it is a 400 (#184
/// round G2).
#[tokio::test]
async fn patch_song_with_dabing_is_400() {
    let _g = SERIAL.lock().await;
    reset_console();
    let state = test_state().await;
    let (status, _) = patch_mix(app(state), r#"{"kind":"song","dabing":0.5}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    reset_console();
}

#[tokio::test]
async fn mix_now_playing_carries_per_song_stems_state() {
    let state = test_state().await;
    let pool = state.pool.clone();
    // A distinct playlist id so this test's now-playing entry is isolated from any
    // set by a parallel test (the registry is process-global).
    insert_playlist(&pool, 771).await;
    let vid = insert_video(&pool, 771, "np_ready").await;
    crate::db::models_stems::mark_stems_done(&pool, vid, "/c/v.flac", "/c/i.flac")
        .await
        .unwrap();
    crate::now_playing::global().set(771, vid);

    let json = get_json(app(state), "/api/v1/mix").await;
    let np = json["now_playing"].as_array().expect("now_playing array");
    let entry = np
        .iter()
        .find(|e| e["playlist_id"] == 771)
        .expect("entry for playlist 771");
    assert_eq!(entry["video_id"], vid);
    assert_eq!(entry["title"], "Song np_ready");
    assert_eq!(entry["stems_state"], "ready");
    assert!(entry["stems_error"].is_null());
    assert!(
        entry["queue_position"].is_null(),
        "ready song is not queued"
    );

    crate::now_playing::global().clear(771);
}

#[tokio::test]
async fn mix_now_playing_reports_queued_position_and_failed_error() {
    let state = test_state().await;
    let pool = state.pool.clone();
    insert_playlist(&pool, 772).await;
    let queued = insert_video(&pool, 772, "np_queued").await;
    crate::now_playing::global().set(772, queued);

    let json = get_json(app(state.clone()), "/api/v1/mix").await;
    let np = json["now_playing"].as_array().unwrap();
    let entry = np.iter().find(|e| e["playlist_id"] == 772).unwrap();
    assert_eq!(entry["stems_state"], "queued");
    assert_eq!(entry["queue_position"], 1);

    // Now fail it → state failed + a human error, no queue position.
    crate::db::models_stems::record_stem_deferral(
        &pool,
        queued,
        std::time::Duration::from_secs(600),
    )
    .await
    .unwrap();
    let json = get_json(app(state), "/api/v1/mix").await;
    let entry = json["now_playing"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["playlist_id"] == 772)
        .unwrap()
        .clone();
    assert_eq!(entry["stems_state"], "failed");
    assert!(entry["stems_error"].as_str().unwrap().contains("zlyhal"));
    assert!(entry["queue_position"].is_null());

    crate::now_playing::global().clear(772);
}

#[tokio::test]
async fn videos_payload_carries_stems_state_marker() {
    let state = test_state().await;
    let pool = state.pool.clone();
    insert_playlist(&pool, 773).await;
    let ready = insert_video(&pool, 773, "vl_ready").await;
    let queued = insert_video(&pool, 773, "vl_queued").await;
    let unsup = insert_video(&pool, 773, "vl_unsup").await;
    crate::db::models_stems::mark_stems_done(&pool, ready, "/c/v.flac", "/c/i.flac")
        .await
        .unwrap();
    crate::db::models_stems::mark_stems_unsupported(&pool, unsup)
        .await
        .unwrap();

    let json = get_json(app(state), "/api/v1/playlists/773/videos").await;
    let rows = json.as_array().unwrap();
    let state_of = |id: i64| {
        rows.iter()
            .find(|v| v["id"] == id)
            .and_then(|v| v["stems_state"].as_str())
            .map(str::to_string)
    };
    assert_eq!(state_of(ready).as_deref(), Some("ready"));
    assert_eq!(state_of(queued).as_deref(), Some("queued"));
    assert_eq!(state_of(unsup).as_deref(), Some("unavailable"));
}

/// #177 mutation: the `stems_error` reason mapping — every arm.
#[test]
fn stems_error_covers_unavailable_failed_and_none() {
    use crate::db::models_stems::StemsState;
    let unavail = super::stems_error(StemsState::Unavailable, 0);
    assert!(
        unavail
            .as_deref()
            .is_some_and(|s| s.contains("nie sú dostupné")),
        "unavailable song must carry its own reason, got {unavail:?}"
    );
    assert_eq!(
        super::stems_error(StemsState::Failed, 3).as_deref(),
        Some("posledný pokus o spracovanie stemov zlyhal (pokusov: 3)"),
    );
    assert!(super::stems_error(StemsState::Ready, 0).is_none());
    assert!(super::stems_error(StemsState::Queued, 5).is_none());
    assert!(super::stems_error(StemsState::Processing, 0).is_none());
}

#[tokio::test]
async fn enqueue_endpoint_reopens_a_failed_song() {
    let state = test_state().await;
    let pool = state.pool.clone();
    insert_playlist(&pool, 774).await;
    let vid = insert_video(&pool, 774, "eq").await;
    crate::db::models_stems::mark_stems_unsupported(&pool, vid)
        .await
        .unwrap();
    assert!(
        crate::db::models_stems::get_next_video_for_stems(&pool)
            .await
            .unwrap()
            .is_none()
    );

    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/stems/{vid}/enqueue"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "enqueued");
    assert_eq!(json["queue_position"], 1);

    assert_eq!(
        crate::db::models_stems::get_next_video_for_stems(&pool)
            .await
            .unwrap()
            .map(|j| j.video_id),
        Some(vid)
    );
}

/// The old karaoke + per-video dub-mix routes are DELETED — they now 404.
#[tokio::test]
async fn deleted_karaoke_and_dub_mix_routes_are_gone() {
    let state = test_state().await;
    let get_karaoke = app(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/karaoke")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_karaoke.status(), StatusCode::NOT_FOUND);

    let dub_mix = app(state)
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/videos/1/dub-mix")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"ratio":1.0}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dub_mix.status(), StatusCode::NOT_FOUND);
}
