//! #265: the block list's routes through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::song_block::BlockedSong;
use tower::ServiceExt;

use crate::AppState;
use crate::api::routes::tests::{app, test_state, test_state_with_engine_rx};

async fn send(state: &AppState, req: Request<Body>) -> (StatusCode, String) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn post(body: &str) -> Request<Body> {
    Request::post("/api/v1/blocked")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn delete(youtube_id: &str) -> Request<Body> {
    Request::delete(format!("/api/v1/blocked/{youtube_id}"))
        .body(Body::empty())
        .unwrap()
}

async fn listed(state: &AppState) -> Vec<BlockedSong> {
    let req = Request::get("/api/v1/blocked").body(Body::empty()).unwrap();
    let (status, body) = send(state, req).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// A song with a name (`song`, `artist`) in a new youtube playlist.
async fn song(state: &AppState, youtube_id: &str, song: &str, artist: &str) -> (i64, i64) {
    let playlist = crate::db::models::insert_playlist(&state.pool, youtube_id, "u")
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, normalized, \
         file_path, audio_file_path) VALUES (?, ?, 'yt title', ?, ?, 1, '/v.mp4', '/a.flac') \
         RETURNING id",
    )
    .bind(playlist.id)
    .bind(youtube_id)
    .bind(song)
    .bind(artist)
    .fetch_one(&state.pool)
    .await
    .unwrap();
    (playlist.id, id)
}

#[tokio::test]
async fn block_list_and_unblock_with_the_server_s_statuses() {
    let state = test_state().await;
    song(&state, "TwzfEwsTfag", "Never Lost", "Elevation").await;
    assert!(listed(&state).await.is_empty());

    let (status, body) = send(&state, post(r#"{"youtube_id":"TwzfEwsTfag"}"#)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let stored: BlockedSong = serde_json::from_str(&body).unwrap();
    assert_eq!(stored.youtube_id, "TwzfEwsTfag");
    assert_eq!(stored.title, "Never Lost - Elevation");
    assert!(stored.blocked_at_ms > 1_700_000_000_000, "{stored:?}");

    // Idempotent: the first block's time stays.
    let (status, body) = send(&state, post(r#"{"youtube_id":" TwzfEwsTfag "}"#)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(serde_json::from_str::<BlockedSong>(&body).unwrap(), stored);
    assert_eq!(listed(&state).await, vec![stored]);

    let (status, _) = send(&state, delete("TwzfEwsTfag")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(listed(&state).await.is_empty());
    let (status, _) = send(&state, delete("TwzfEwsTfag")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not blocked any more");
}

#[tokio::test]
async fn a_song_this_node_does_not_have_is_404_and_nothing_is_stored() {
    let state = test_state().await;
    let (status, _) = send(&state, post(r#"{"youtube_id":"aaaaaaaaaaa"}"#)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(listed(&state).await.is_empty());
}

#[tokio::test]
async fn a_body_without_a_youtube_id_is_refused() {
    let state = test_state().await;
    let (status, _) = send(&state, post(r#"{"video_id":1}"#)).await;
    assert!(status.is_client_error(), "{status}");
    assert!(listed(&state).await.is_empty());
}

/// An operator's ▶ of a blocked song by id still plays it.
#[tokio::test]
async fn a_play_by_id_of_a_blocked_song_still_plays() {
    let (state, mut engine_rx) = test_state_with_engine_rx().await;
    let (playlist, id) = song(&state, "TwzfEwsTfag", "Never Lost", "Elevation").await;
    let (status, _) = send(&state, post(r#"{"youtube_id":"TwzfEwsTfag"}"#)).await;
    assert_eq!(status, StatusCode::CREATED);
    let req = Request::post(format!("/api/v1/playlists/{playlist}/play-video"))
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"video_id": {id}}}"#)))
        .unwrap();
    let (status, body) = send(&state, req).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(
        engine_rx.try_recv().is_ok(),
        "the engine is told to play it"
    );
}
