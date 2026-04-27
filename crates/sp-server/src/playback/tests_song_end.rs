//! Regression tests for the song-end Presenter clear.
//!
//! Included as a sibling file via `#[path = "tests_song_end.rs"] mod ...`
//! from `playback/mod.rs` to keep that file under the 1000-line airuleset cap.

use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::presenter::PresenterClient;

/// When a pipeline fires `PipelineEvent::Ended` the engine MUST push an
/// empty PresenterPayload (all four fields empty strings) so the stage
/// display clears. Before the fix, the display kept showing the last
/// line of the previous song until the next song's first line pushed —
/// band singers got stuck on an old verse.
#[tokio::test]
async fn presenter_empty_payload_on_song_end() {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (77, 'p', 'u')")
        .execute(&pool)
        .await
        .unwrap();

    // Mock server that expects exactly one PUT /api/stage with the empty
    // payload (all four fields = ""). The expect(1) is the assertion —
    // wiremock verifies it on drop.
    let mock = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/api/stage"))
        .and(body_json(serde_json::json!({
            "currentText": "",
            "nextText": "",
            "currentSong": "",
            "nextSong": ""
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&mock)
        .await;

    let presenter_client = Arc::new(PresenterClient::new(format!("{}/api/stage", mock.uri())));

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache-song-end"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
        Some(presenter_client),
        std::sync::Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    );
    engine.ensure_pipeline(77, "SP-test");

    // Fire Ended — handler should call clear_lyrics_display which
    // spawns the empty-PUT.
    engine.handle_pipeline_event(77, PipelineEvent::Ended).await;

    // The Presenter push is fire-and-forget (`tokio::spawn`) — wait up to
    // 2 s for wiremock to record the call before assertions fire on drop.
    for _ in 0..20 {
        let recv = mock.received_requests().await;
        if let Some(reqs) = recv {
            if !reqs.is_empty() {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Dropping `mock` at end of scope runs the `.expect(1)` check.
    // If zero PUTs arrived, the test fails with a clear wiremock error.
    drop(mock);
}
