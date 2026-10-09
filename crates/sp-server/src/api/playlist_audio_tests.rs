//! #242 `GET`/`PUT /api/v1/playlists/{id}/audio` through the real router:
//! the row and the live register follow a valid PUT, a refused one writes
//! nothing, an unknown playlist is 404.
//! Wired via `#[cfg(test)] #[path = "playlist_audio_tests.rs"] mod tests;`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::audio_fx::{BandKind, EqBand, PlaylistFx};
use tower::ServiceExt;

use crate::AppState;
use crate::api::routes::tests::{app, test_state};
use crate::db::models_playlist_fx::get_playlist_fx;
use crate::playback::playlist_fx::global;

async fn state_with(id: i64) -> AppState {
    let state = test_state().await;
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (?, 'p', ?)")
        .bind(id)
        .bind(format!("u{id}"))
        .execute(&state.pool)
        .await
        .unwrap();
    state
}

async fn send(state: &AppState, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

fn shaped() -> PlaylistFx {
    PlaylistFx {
        gain_db: -7.5,
        eq: vec![EqBand {
            kind: BandKind::LowShelf,
            freq_hz: 150.0,
            gain_db: -18.0,
            q: 0.707,
            enabled: true,
        }],
    }
}

#[tokio::test]
async fn a_valid_put_writes_the_row_then_the_live_register() {
    let id = 242_001;
    let state = state_with(id).await;
    let before = global().slot(id).generation();
    let (status, _) = send(
        &state,
        "PUT",
        &format!("/api/v1/playlists/{id}/audio"),
        r#"{"gain_db":-7.5,"eq":[{"kind":"low_shelf","freq_hz":150,"gain_db":-18,"q":0.707}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        get_playlist_fx(&state.pool, id).await.unwrap(),
        Some(shaped())
    );
    let (generation, live) = global().slot(id).read();
    assert_eq!((generation, live), (before + 1, shaped()));
    let (status, body) = send(&state, "GET", &format!("/api/v1/playlists/{id}/audio"), "").await;
    assert_eq!(status, StatusCode::OK);
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["gain_db"], -7.5);
    assert_eq!(view["eq"][0]["kind"], "low_shelf");
    assert_eq!(view["eq"][0]["freq_hz"], 150.0);
    assert_eq!(view["eq"][0]["enabled"], true);
    assert_eq!(view["generation"], before + 1);
}

/// A body that does not read, an unknown field or a value past a limit is
/// 400 with its reason, and neither the row nor the register moves.
#[tokio::test]
async fn a_refused_put_writes_nothing_and_names_the_reason() {
    let id = 242_002;
    let state = state_with(id).await;
    let before = global().slot(id).generation();
    for (body, reason) in [
        ("not json", "audio body"),
        (r#"{"gain_db":0,"bands":[]}"#, "unknown field"),
        (r#"{"gain_db":-31}"#, "audio_gain_db -31 is outside"),
        (
            r#"{"eq":[{"kind":"peak","freq_hz":19}]}"#,
            "audio_eq band 0: freq_hz 19 is outside",
        ),
        (r#"{"eq":[{"kind":"wobble","freq_hz":100}]}"#, "audio body"),
    ] {
        let (status, text) = send(
            &state,
            "PUT",
            &format!("/api/v1/playlists/{id}/audio"),
            body,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(text.contains(reason), "{body}: {text}");
    }
    assert_eq!(
        get_playlist_fx(&state.pool, id).await.unwrap(),
        Some(PlaylistFx::default())
    );
    assert_eq!(global().slot(id).generation(), before);
}

#[tokio::test]
async fn an_unknown_playlist_is_404_both_ways() {
    let state = test_state().await;
    let (status, _) = send(&state, "GET", "/api/v1/playlists/242999/audio", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let before = global().slot(242_999).generation();
    let (status, _) = send(
        &state,
        "PUT",
        "/api/v1/playlists/242999/audio",
        r#"{"gain_db":-3}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(global().slot(242_999).generation(), before, "nothing set");
}
