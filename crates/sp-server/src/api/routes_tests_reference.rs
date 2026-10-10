//! ★ reference marker tests for `api::routes` (#142) — `lyrics_reference`
//! field exposure on the lyrics-songs list, plus the feedback and
//! admin-toggle endpoints. Split out of `routes_tests.rs` to keep it under
//! the 1000-line airuleset cap. Included as a sibling file via
//! `#[path = "routes_tests_reference.rs"] #[cfg(test)] mod tests_reference;`
//! from routes.rs; shares `test_state`/`app` with `routes_tests.rs` via
//! `super::tests`.

#![allow(unused_imports)]

use super::tests::{app, test_state};
use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn list_lyrics_songs_exposes_lyrics_reference() {
    // #142: the dashboard's ★ badge reads this field from the lyrics-songs
    // response to decide which rows carry Claude's verified reference lyrics.
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_reference) \
         VALUES (1, 1, 'yt-ref', 1, 1), (2, 1, 'yt-noref', 1, 0)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let app = app(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/lyrics/songs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(items.len(), 2);

    let referenced = items
        .iter()
        .find(|v| v["video_id"] == 1)
        .expect("row for video 1");
    let not_referenced = items
        .iter()
        .find(|v| v["video_id"] == 2)
        .expect("row for video 2");
    assert_eq!(
        referenced["lyrics_reference"],
        serde_json::Value::Bool(true),
        "video 1 must serialize lyrics_reference=true"
    );
    assert_eq!(
        not_referenced["lyrics_reference"],
        serde_json::Value::Bool(false),
        "video 2 must serialize lyrics_reference=false"
    );
}

/// #144 F4: the owner's „Nesedí" mark (when, and the note) reads through the
/// song list and the song detail; a song never marked shows none.
#[tokio::test]
async fn the_lyrics_song_routes_expose_the_owners_rejection_mark() {
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_reference) \
         VALUES (1, 1, 'yt-marked', 1, 1), (2, 1, 'yt-clean', 1, 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    crate::db::models::record_reference_feedback(&state.pool, 1, "refrén nesedí")
        .await
        .unwrap();

    let get = |uri: &'static str| {
        let app = app(state.clone());
        async move {
            let resp = app
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        }
    };
    let items = get("/api/v1/lyrics/songs").await;
    let marked = items
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["video_id"] == 1)
        .expect("row for video 1")
        .clone();
    let clean = items
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["video_id"] == 2)
        .expect("row for video 2")
        .clone();
    let at = marked["reference_rejected_at"].as_str().expect("a time");
    assert!(at.starts_with("20") && at.ends_with('Z'), "{at}");
    assert_eq!(marked["reference_note"], "refrén nesedí");
    assert_eq!(marked["lyrics_reference"], false, "the mark clears the ★");
    assert!(clean["reference_rejected_at"].is_null());
    assert!(clean["reference_note"].is_null());

    let detail = get("/api/v1/lyrics/songs/1").await;
    assert_eq!(detail["list_item"]["reference_rejected_at"], at);
    assert_eq!(detail["list_item"]["reference_note"], "refrén nesedí");
}

// ── #142 — ★ reference marker: feedback + admin toggle endpoints ──────────

#[tokio::test]
async fn reference_feedback_endpoint_clears_flag_and_records_note() {
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, lyrics_reference) \
         VALUES (50, 1, 'ytREF', 1, 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/50/reference-feedback")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"note": "refrén nesedí s videom"}))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let row = sqlx::query(
        "SELECT lyrics_reference, lyrics_reference_rejected_at, lyrics_reference_note, \
         lyrics_manual_priority FROM videos WHERE id = 50",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap();
    let reference: i64 = row.get("lyrics_reference");
    let rejected_at: Option<String> = row.get("lyrics_reference_rejected_at");
    let note: Option<String> = row.get("lyrics_reference_note");
    let manual_priority: i64 = row.get("lyrics_manual_priority");
    assert_eq!(reference, 0, "star must be cleared");
    assert!(rejected_at.is_some(), "rejected_at must be stamped");
    assert_eq!(note, Some("refrén nesedí s videom".to_string()));
    assert_eq!(manual_priority, 1, "song must be re-queued");
}

#[tokio::test]
async fn reference_feedback_endpoint_returns_404_for_missing_video() {
    let state = test_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/999/reference-feedback")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"note": "x"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_reference_endpoint_toggles_flag_true_and_false() {
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized) VALUES (60, 1, 'ytSET', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/60/reference")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"reference": true})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let on: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = 60")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(on, 1);

    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/60/reference")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"reference": false})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let off: i64 = sqlx::query_scalar("SELECT lyrics_reference FROM videos WHERE id = 60")
        .fetch_one(&state.pool)
        .await
        .unwrap();
    assert_eq!(off, 0);
}

#[tokio::test]
async fn set_reference_endpoint_returns_404_for_missing_video() {
    let state = test_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/lyrics/songs/999/reference")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"reference": true})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// #144 F4: the list and the detail share ONE row reader (`list_item_from`);
/// each flag is read from its own column. Three rows make every comparison
/// of the reader observable: a stale, queued, suppressed ★ row; a current
/// row with none of them; a row with no lyrics at version 0 (never stale).
#[tokio::test]
async fn the_song_list_reads_each_flag_from_its_row() {
    let state = test_state().await;
    let current = i64::from(crate::lyrics::LYRICS_PIPELINE_VERSION);
    sqlx::query("INSERT INTO playlists (id, name, youtube_url, is_active) VALUES (1, 'p', 'u', 1)")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized, has_lyrics, \
         lyrics_pipeline_version, lyrics_manual_priority, suppress_resolume_en, \
         lyrics_reference) VALUES \
         (1, 1, 'yt-a', 1, 1, ?1, 1, 1, 1), \
         (2, 1, 'yt-b', 1, 1, ?2, 0, 0, 0), \
         (3, 1, 'yt-c', 1, 0, 0, 0, 0, 0)",
    )
    .bind(current - 1)
    .bind(current)
    .execute(&state.pool)
    .await
    .unwrap();
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri("/api/v1/lyrics/songs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let items: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    let flags = |id: i64| {
        let v = items.iter().find(|v| v["video_id"] == id).expect("row");
        [
            v["has_lyrics"].as_bool().unwrap(),
            v["is_stale"].as_bool().unwrap(),
            v["manual_priority"].as_bool().unwrap(),
            v["suppress_resolume_en"].as_bool().unwrap(),
            v["lyrics_reference"].as_bool().unwrap(),
        ]
    };
    assert_eq!(flags(1), [true, true, true, true, true]);
    assert_eq!(flags(2), [true, false, false, false, false]);
    assert_eq!(flags(3), [false, false, false, false, false]);
}

/// #144 F1: the operator's override text, PATCHed on one row, reaches every
/// row of the video (a pass of any row serves every row); a blank one clears
/// every row.
#[tokio::test]
async fn an_override_text_patched_on_one_row_reaches_every_row_of_the_video() {
    let state = test_state().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, is_active) VALUES \
         (1, 'p', 'u', 1), (2, 'q', 'u2', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, normalized) VALUES \
         (1, 1, 'yt-shared', 1), (2, 2, 'yt-shared', 1), (3, 1, 'yt-other', 1)",
    )
    .execute(&state.pool)
    .await
    .unwrap();
    for (text, want) in [("Moj text", Some("Moj text")), ("  ", None)] {
        let resp = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/videos/2")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"lyrics_override_text": text}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "{text:?}");
        let texts: Vec<Option<String>> =
            sqlx::query_scalar("SELECT lyrics_override_text FROM videos ORDER BY id")
                .fetch_all(&state.pool)
                .await
                .unwrap();
        assert_eq!(
            texts,
            vec![want.map(str::to_string), want.map(str::to_string), None],
            "{text:?}"
        );
    }
}
