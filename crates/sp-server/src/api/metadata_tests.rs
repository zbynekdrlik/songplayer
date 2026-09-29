//! #136: `status.metadata` and `POST /api/v1/metadata/probe`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use sp_core::metadata::{MetadataSource, VideoMetadata};
use sqlx::Row;
use tower::ServiceExt;

use super::*;
use crate::api::routes::tests::{app, test_state};
use crate::metadata::health::{MAX_ERROR_CHARS, ProviderHealth};
use crate::metadata::test_support::{ARTIST, SONG, TITLE, VIDEO};
use crate::metadata::{MetadataError, ProviderChain, get_metadata};

/// Refuses every video (named like the chain's first provider).
struct Refuses;

#[async_trait]
impl MetadataProvider for Refuses {
    async fn extract(&self, _video_id: &str, _title: &str) -> Result<VideoMetadata, MetadataError> {
        Err(MetadataError::ApiError(
            "chat completion failed (HTTP 502)".into(),
        ))
    }

    fn name(&self) -> &str {
        "claude"
    }
}

/// Answers every video with SONG / ARTIST (named like the chain's second
/// provider).
struct Answers;

#[async_trait]
impl MetadataProvider for Answers {
    async fn extract(&self, _video_id: &str, _title: &str) -> Result<VideoMetadata, MetadataError> {
        Ok(VideoMetadata {
            song: SONG.into(),
            artist: ARTIST.into(),
            source: MetadataSource::Gemini,
            gemini_failed: false,
        })
    }

    fn name(&self) -> &str {
        "gemini"
    }
}

fn refuses_then_answers() -> Arc<ProviderChain> {
    Arc::new(ProviderChain::new(vec![
        Box::new(Refuses),
        Box::new(Answers),
    ]))
}

async fn insert_video(state: &AppState, youtube_id: &str, gemini_failed: i64) {
    sqlx::query("INSERT OR IGNORE INTO playlists (id, name, youtube_url) VALUES (7, 'P', 'url')")
        .execute(&state.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, gemini_failed, normalized)
         VALUES (7, ?, ?, ?, '', ?, 1)",
    )
    .bind(youtube_id)
    .bind(TITLE)
    .bind(TITLE)
    .bind(gemini_failed)
    .execute(&state.pool)
    .await
    .unwrap();
}

async fn send(state: AppState, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = app(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, body.to_vec())
}

fn probe_request(youtube_id: &str, title: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/metadata/probe")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"youtube_id": youtube_id, "title": title}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn status_reports_the_repair_queue_and_every_provider_of_the_chain() {
    let mut state = test_state().await;
    state.metadata_chain = refuses_then_answers();
    insert_video(&state, "a1", 1).await;
    insert_video(&state, "a2", 1).await;
    insert_video(&state, "a3", 0).await;
    let before = chrono::Utc::now().timestamp_millis();
    get_metadata(state.metadata_chain.providers(), VIDEO, TITLE).await;

    let (status, body) = send(
        state,
        Request::builder()
            .uri("/api/v1/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let json: crate::api::routes::StatusResponse = serde_json::from_slice(&body).unwrap();
    let metadata = json.metadata;
    assert_eq!(metadata.failed_videos, Some(2));
    let [claude, gemini] = metadata.providers.as_slice() else {
        panic!("two providers in chain order: {:?}", metadata.providers);
    };
    assert_eq!(
        claude,
        &ProviderHealth {
            name: "claude".into(),
            last_ok_at_ms: None,
            last_error: Some("API request failed: chat completion failed (HTTP 502)".into()),
        }
    );
    assert_eq!(gemini.name, "gemini");
    assert_eq!(gemini.last_error, None);
    assert!(
        gemini.last_ok_at_ms.is_some_and(|at| at >= before),
        "{gemini:?}"
    );
}

#[tokio::test]
async fn status_reports_no_count_rather_than_a_false_zero_when_the_db_cannot_be_read() {
    let mut state = test_state().await;
    state.metadata_chain = refuses_then_answers();
    state.pool.close().await;

    let block = status_block(&state).await;

    assert_eq!(block.failed_videos, None);
    assert_eq!(block.providers.len(), 2);
}

#[tokio::test]
async fn the_probe_asks_every_provider_on_its_own_and_writes_nothing() {
    let mut state = test_state().await;
    state.metadata_chain = refuses_then_answers();
    insert_video(&state, VIDEO, 1).await;
    let pool = state.pool.clone();
    let chain = Arc::clone(&state.metadata_chain);

    let (status, body) = send(state, probe_request(VIDEO, TITLE)).await;

    assert_eq!(status, StatusCode::OK);
    let resp: ProbeResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(resp.youtube_id, VIDEO);
    let names: Vec<&str> = resp.providers.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["claude", "gemini"], "chain order, every provider");
    let [claude, gemini] = resp.providers.as_slice() else {
        unreachable!()
    };
    assert!(!claude.ok);
    assert_eq!(
        (claude.song.as_deref(), claude.artist.as_deref()),
        (None, None)
    );
    assert_eq!(
        claude.error.as_deref(),
        Some("API request failed: chat completion failed (HTTP 502)")
    );
    // Every provider answers for itself, whatever the one before it did.
    assert!(gemini.ok);
    assert_eq!(
        (
            gemini.song.as_deref(),
            gemini.artist.as_deref(),
            gemini.error.as_deref()
        ),
        (Some(SONG), Some(ARTIST), None)
    );
    // Nothing written: the row still carries the parser's title.
    let row = sqlx::query("SELECT song, artist, gemini_failed FROM videos WHERE youtube_id = ?")
        .bind(VIDEO)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("song"), TITLE);
    assert_eq!(row.get::<String, _>("artist"), "");
    assert_eq!(row.get::<i64, _>("gemini_failed"), 1);
    // A probe is a real call: it lands in the providers' health.
    let health = chain.health();
    assert!(health[0].last_error.is_some() && health[1].last_ok_at_ms.is_some());
}

#[tokio::test]
async fn a_probe_without_a_video_id_or_a_title_is_a_bad_request() {
    for (youtube_id, title) in [(VIDEO, " "), ("", TITLE)] {
        let mut state = test_state().await;
        state.metadata_chain = refuses_then_answers();
        let chain = Arc::clone(&state.metadata_chain);

        let (status, _) = send(state, probe_request(youtube_id, title)).await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{youtube_id:?} / {title:?}"
        );
        assert!(
            chain
                .health()
                .iter()
                .all(|h| h.last_ok_at_ms.is_none() && h.last_error.is_none()),
            "a refused probe asks no provider"
        );
    }
}

/// Never answers within a probe's bound.
struct Hangs;

#[async_trait]
impl MetadataProvider for Hangs {
    async fn extract(&self, _video_id: &str, _title: &str) -> Result<VideoMetadata, MetadataError> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Err(MetadataError::ApiError("too late".into()))
    }

    fn name(&self) -> &str {
        "claude"
    }
}

/// Fails with a very long error (a proxy echoing its whole reply).
struct Verbose;

#[async_trait]
impl MetadataProvider for Verbose {
    async fn extract(&self, _video_id: &str, _title: &str) -> Result<VideoMetadata, MetadataError> {
        Err(MetadataError::ApiError("x".repeat(1_000)))
    }

    fn name(&self) -> &str {
        "gemini"
    }
}

#[tokio::test]
async fn a_provider_that_does_not_answer_in_time_is_reported_by_name() {
    let outcome = probe_one(&Hangs, VIDEO, TITLE, Duration::from_millis(20)).await;

    assert_eq!(outcome.name, "claude");
    assert!(!outcome.ok);
    assert_eq!(outcome.error.as_deref(), Some("no answer within 0.02 s"));
    assert_eq!((outcome.song, outcome.artist), (None, None));
}

#[tokio::test]
async fn a_long_provider_error_is_cut_in_the_probe_answer() {
    let outcome = probe_one(&Verbose, VIDEO, TITLE, PROBE_TIMEOUT).await;

    assert!(!outcome.ok);
    let error = outcome.error.expect("the provider's error");
    assert_eq!(error.chars().count(), MAX_ERROR_CHARS);
}

#[test]
fn the_probe_bound_stays_below_the_post_deploy_request_timeout() {
    // e2e/post-deploy-metadata.spec.ts waits 220 s for the probe answer.
    assert_eq!(PROBE_TIMEOUT, Duration::from_secs(180));
}

#[tokio::test]
async fn a_probe_with_an_over_long_id_or_title_is_a_bad_request() {
    let long_id = "i".repeat(MAX_PROBE_ID_CHARS + 1);
    let long_title = "t".repeat(MAX_PROBE_TITLE_CHARS + 1);
    for (youtube_id, title) in [(long_id.as_str(), TITLE), (VIDEO, long_title.as_str())] {
        let mut state = test_state().await;
        state.metadata_chain = refuses_then_answers();

        let (status, _) = send(state, probe_request(youtube_id, title)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{}", title.len());
    }
}

#[tokio::test]
async fn a_probe_at_the_length_bounds_is_answered() {
    let id = "i".repeat(MAX_PROBE_ID_CHARS);
    let title = "t".repeat(MAX_PROBE_TITLE_CHARS);
    let mut state = test_state().await;
    state.metadata_chain = refuses_then_answers();

    let (status, _) = send(state, probe_request(&id, &title)).await;

    assert_eq!(status, StatusCode::OK);
}
