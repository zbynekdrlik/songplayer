//! #144: Claude's content filter refusing a song's translation is final —
//! the song keeps its English lines and the next song goes on; an outage
//! still backs off. Claude is a wiremock that refuses every request naming
//! the refused song's line.

use std::sync::Arc;

use sp_core::lyrics::{LyricsLine, LyricsTrack};
use tokio::sync::broadcast;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::ai::AiSettings;
use crate::ai::client::AiClient;
use crate::ai::retry::RetryPolicy;

const FILTER_BODY: &str = r#"{"error":{"message":"claude executor: upstream returned error event: Output blocked by content filtering policy","type":"server_error"}}"#;

fn untranslated(en: &str) -> LyricsTrack {
    LyricsTrack {
        version: 1,
        source: "mtl+g35t".into(),
        language_source: "en".into(),
        language_translation: String::new(),
        lines: vec![LyricsLine {
            start_ms: 0,
            end_ms: 1000,
            en: en.into(),
            sk: None,
            words: None,
        }],
    }
}

/// A Claude that refuses (content filter, or `refusal_status` with no
/// filter body) every request naming "refused line", and translates the
/// rest.
async fn claude(refusal_status: u16, refusal_body: &'static str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("refused line"))
        .respond_with(ResponseTemplate::new(refusal_status).set_body_string(refusal_body))
        .with_priority(1)
        .mount(&server)
        .await;
    let ok = serde_json::json!({"choices": [{"message": {"content": "1: Preložené"}}]});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok))
        .mount(&server)
        .await;
    server
}

/// Two active songs with lyrics and no SK line — "refused line" first (the
/// pass picks by id), "free line" second — and a worker on `server`.
async fn two_songs(server: &MockServer) -> (LyricsWorker, tempfile::TempDir, i64) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (1, 'p', 'u', 'n', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut first = 0;
    for (yt, en) in [("yt_refused", "refused line"), ("yt_free", "free line")] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, normalized, \
             has_lyrics) VALUES (1, ?, 'T', 'S', 'A', 1, 1) RETURNING id",
        )
        .bind(yt)
        .fetch_one(&pool)
        .await
        .unwrap();
        if first == 0 {
            first = id;
        }
        let json = serde_json::to_vec(&untranslated(en)).unwrap();
        std::fs::write(dir.path().join(format!("{yt}_lyrics.json")), json).unwrap();
    }
    let (events, _) = broadcast::channel(16);
    let mut worker = LyricsWorker::new_for_test(pool, dir.path().to_path_buf(), events);
    worker.ai_client = Some(Arc::new(
        AiClient::new(AiSettings {
            api_url: format!("{}/v1", server.uri()),
            api_key: None,
            model: "claude-test".into(),
            system_prompt_extra: None,
        })
        .with_retry_policy(RetryPolicy::NO_WAIT),
    ));
    (worker, dir, first)
}

fn sk(dir: &tempfile::TempDir, yt: &str) -> Option<String> {
    let json = std::fs::read(dir.path().join(format!("{yt}_lyrics.json"))).unwrap();
    let track: LyricsTrack = serde_json::from_slice(&json).unwrap();
    track.lines[0].sk.clone()
}

#[tokio::test]
async fn a_refused_translation_is_final_and_the_next_song_goes_on() {
    let server = claude(502, FILTER_BODY).await;
    let (worker, dir, _) = two_songs(&server).await;
    worker.retry_missing_translations().await;
    worker.retry_missing_translations().await;
    assert_eq!(sk(&dir, "yt_refused"), None, "keeps its English lines");
    assert_eq!(sk(&dir, "yt_free").as_deref(), Some("Preložené"));
    worker.retry_missing_translations().await;
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "the refused song is never sent again this run"
    );
    let backoff = worker.retry_backoff.lock().await;
    assert!(backoff.silent_until.is_none(), "no backoff for a refusal");
    assert!(backoff.refused_translations.contains("yt_refused"));
}

#[tokio::test]
async fn an_outage_still_backs_off_and_is_asked_again_later() {
    let server = claude(500, "upstream down").await;
    let (worker, dir, _) = two_songs(&server).await;
    worker.retry_missing_translations().await;
    worker.retry_missing_translations().await;
    assert_eq!(sk(&dir, "yt_free"), None, "the backoff holds the pass");
    let backoff = worker.retry_backoff.lock().await;
    assert!(backoff.silent_until.is_some());
    assert!(backoff.refused_translations.is_empty());
}

#[tokio::test]
async fn a_refused_stale_translation_is_stamped_forward() {
    let server = claude(502, FILTER_BODY).await;
    let (worker, _dir, refused) = two_songs(&server).await;
    worker.retranslate_next_stale().await;
    let version: i64 =
        sqlx::query_scalar("SELECT lyrics_translation_version FROM videos WHERE id = ?")
            .bind(refused)
            .fetch_one(&worker.pool)
            .await
            .unwrap();
    assert_eq!(
        version,
        i64::from(crate::lyrics::LYRICS_TRANSLATION_VERSION)
    );
    assert!(worker.retry_backoff.lock().await.silent_until.is_none());
}
