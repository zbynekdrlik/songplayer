//! #136: the reprocess worker on the ONE production provider chain.
//!
//! Before #136 `lib.rs` gave this worker Gemini ALONE (the download worker had
//! Claude + Gemini), and Gemini sent the whole key list as one key, so ~110
//! rows kept their raw YouTube title for good while Claude answered correctly.

use super::*;
use crate::db;
use crate::metadata::chain::provider_chain_at;
use crate::metadata::test_support::{
    ARTIST, SONG, TITLE, VIDEO, ai_client_at, claude_answers, claude_refuses, gemini_answer,
    key_invalid_body, received_keys,
};
use crate::metadata::{MetadataError, MetadataProvider};
use async_trait::async_trait;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn pool_with_parser_row() -> (SqlitePool, i64) {
    let pool = db::create_memory_pool().await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    sqlx::query("INSERT INTO playlists (id, name, youtube_url) VALUES (7, 'ytfast', 'url')")
        .execute(&pool)
        .await
        .unwrap();
    // The live row of the #136 report: the whole title as `song`, no artist.
    let id: i64 = sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, song, artist, gemini_failed,
                             normalized, file_path, metadata_source)
         VALUES (7, ?, ?, ?, '', 1, 1, '', 'regex') RETURNING id",
    )
    .bind(VIDEO)
    .bind(TITLE)
    .bind(TITLE)
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("id");
    (pool, id)
}

async fn row(pool: &SqlitePool, id: i64) -> (String, String, i64) {
    let r = sqlx::query("SELECT song, artist, gemini_failed FROM videos WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    (r.get("song"), r.get("artist"), r.get("gemini_failed"))
}

#[tokio::test]
async fn an_answering_claude_repairs_the_row_through_the_production_chain() {
    let (pool, id) = pool_with_parser_row().await;
    let proxy = MockServer::start().await;
    claude_answers(&proxy, SONG, ARTIST).await;
    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&google)
        .await;
    let chain = provider_chain_at(ai_client_at(&proxy), "k1,k2", "gemini-test", &google.uri());
    let mut worker = ReprocessWorker::new(pool.clone(), chain, PathBuf::from("."));

    assert_eq!(worker.process_all().await.unwrap(), 1);

    assert_eq!(row(&pool, id).await, (SONG.into(), ARTIST.into(), 0));
}

#[tokio::test]
async fn a_refusing_claude_repairs_the_row_through_gemini_on_the_next_key() {
    let (pool, id) = pool_with_parser_row().await;
    let proxy = MockServer::start().await;
    claude_refuses(&proxy).await;
    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-goog-api-key", "k1"))
        .respond_with(ResponseTemplate::new(400).set_body_json(key_invalid_body()))
        .mount(&google)
        .await;
    Mock::given(method("POST"))
        .and(header("x-goog-api-key", "k2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_answer(SONG, ARTIST)))
        .mount(&google)
        .await;
    let chain = provider_chain_at(ai_client_at(&proxy), "k1,k2", "gemini-test", &google.uri());
    let mut worker = ReprocessWorker::new(pool.clone(), chain.clone(), PathBuf::from("."));

    assert_eq!(worker.process_all().await.unwrap(), 1);

    assert_eq!(row(&pool, id).await, (SONG.into(), ARTIST.into(), 0));
    assert_eq!(received_keys(&google).await, ["k1", "k2", "k2"]);
    assert!(chain.health()[1].last_ok_at_ms.is_some());
}

/// A provider that always fails with its own name and message.
struct Refusing {
    name: &'static str,
    error: MetadataError,
}

#[async_trait]
impl MetadataProvider for Refusing {
    async fn extract(&self, _video_id: &str, _title: &str) -> Result<VideoMetadata, MetadataError> {
        Err(match &self.error {
            MetadataError::ApiError(m) => MetadataError::ApiError(m.clone()),
            MetadataError::InvalidResponse(m) => MetadataError::InvalidResponse(m.clone()),
            MetadataError::RateLimited(m) => MetadataError::RateLimited(m.clone()),
        })
    }

    fn name(&self) -> &str {
        self.name
    }
}

fn chain_of(providers: Vec<Refusing>) -> Arc<ProviderChain> {
    Arc::new(ProviderChain::new(
        providers
            .into_iter()
            .map(|p| Box::new(p) as Box<dyn MetadataProvider>)
            .collect(),
    ))
}

#[tokio::test]
async fn a_failed_rows_reason_names_every_provider_in_chain_order() {
    let (pool, id) = pool_with_parser_row().await;
    let chain = chain_of(vec![
        Refusing {
            name: "claude",
            error: MetadataError::ApiError("proxy down".into()),
        },
        Refusing {
            name: "gemini",
            error: MetadataError::ApiError("all 5 keys failed".into()),
        },
    ]);
    let worker = ReprocessWorker::new(pool.clone(), chain, PathBuf::from("."));

    let failure = worker.try_providers(VIDEO, TITLE).await.unwrap_err();

    assert_eq!(
        failure.reasons,
        "claude: API request failed: proxy down; gemini: API request failed: all 5 keys failed"
    );
    assert!(!failure.rate_limited);
    assert_eq!(row(&pool, id).await, (TITLE.into(), String::new(), 1));
}

#[tokio::test]
async fn a_rate_limited_provider_marks_the_failure_and_keeps_every_reason() {
    let (pool, _) = pool_with_parser_row().await;
    let chain = chain_of(vec![
        Refusing {
            name: "claude",
            error: MetadataError::RateLimited("proxy quota".into()),
        },
        Refusing {
            name: "gemini",
            error: MetadataError::InvalidResponse("no text".into()),
        },
    ]);
    let worker = ReprocessWorker::new(pool, chain, PathBuf::from("."));

    let failure = worker.try_providers(VIDEO, TITLE).await.unwrap_err();

    assert!(failure.rate_limited);
    assert_eq!(
        failure.reasons,
        "claude: rate limited: proxy quota; gemini: Invalid response: no text"
    );
}

#[tokio::test]
async fn an_empty_chain_fails_with_a_reason() {
    let (pool, _) = pool_with_parser_row().await;
    let worker = ReprocessWorker::new(pool, chain_of(vec![]), PathBuf::from("."));

    let failure = worker.try_providers(VIDEO, TITLE).await.unwrap_err();

    assert_eq!(failure.reasons, "no providers configured");
    assert!(!failure.rate_limited);
}
