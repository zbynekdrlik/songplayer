//! #136: the ONE production provider chain.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::metadata::get_metadata;
use crate::metadata::test_support::{
    ARTIST, SONG, TITLE, VIDEO, ai_client_at, claude_answers, claude_refuses, gemini_answer,
    received_keys,
};
use sp_core::metadata::MetadataSource;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn the_production_chain_is_claude_then_gemini() {
    let proxy = MockServer::start().await;
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let chain = provider_chain(&pool, ai_client_at(&proxy), "k1,k2", "gemini-test");

    let providers = chain.providers().await.expect("paid AI on: the providers");
    let names: Vec<&str> = providers.iter().map(|p| p.name()).collect();
    assert_eq!(names, ["claude", "gemini"]);
    let health = chain.health();
    let health_names: Vec<&str> = health.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(health_names, ["claude", "gemini"]);
    assert!(
        health
            .iter()
            .all(|h| h.last_ok_at_ms.is_none() && h.last_error.is_none()),
        "no call yet: {health:?}"
    );
}

/// The download worker's path (`get_metadata`) on the production chain: a
/// refusing Claude falls through to Gemini, which sends ONE key of the
/// `gemini_api_key` list (the whole CSV before #136 → refused by Google →
/// the title parser's "Stand On Your Promise by The Emerging Sound (feat. …)").
#[tokio::test]
async fn a_refusing_claude_falls_through_to_gemini_on_one_key_of_the_list() {
    let proxy = MockServer::start().await;
    claude_refuses(&proxy).await;
    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-goog-api-key", "k1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_answer(SONG, ARTIST)))
        .mount(&google)
        .await;
    let chain = provider_chain_at(
        ai_client_at(&proxy),
        " k1 , k2 ",
        "gemini-test",
        &google.uri(),
    );

    let meta = get_metadata(chain.providers().await.unwrap(), VIDEO, TITLE).await;

    assert_eq!(meta.song, SONG);
    assert_eq!(meta.artist, ARTIST);
    assert_eq!(meta.source, MetadataSource::Gemini);
    assert!(!meta.gemini_failed);
    assert_eq!(received_keys(&google).await, ["k1", "k1"]);
    let health = chain.health();
    let claude_error = health[0].last_error.as_deref().unwrap_or_default();
    assert!(claude_error.contains("HTTP 400"), "{health:?}");
    assert_eq!(health[0].last_ok_at_ms, None);
    assert!(health[1].last_ok_at_ms.is_some(), "{health:?}");
    assert_eq!(health[1].last_error, None);
}

#[tokio::test]
async fn an_answering_claude_is_asked_first_and_gemini_not_at_all() {
    let proxy = MockServer::start().await;
    claude_answers(&proxy, SONG, ARTIST).await;
    let google = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&google)
        .await;
    let chain = provider_chain_at(ai_client_at(&proxy), "k1", "gemini-test", &google.uri());

    let meta = get_metadata(chain.providers().await.unwrap(), VIDEO, TITLE).await;

    assert_eq!((meta.song.as_str(), meta.artist.as_str()), (SONG, ARTIST));
    assert!(!meta.gemini_failed);
    assert!(chain.health()[0].last_ok_at_ms.is_some());
}

/// Fails its first call, answers every later one.
struct FailsOnce {
    failed: AtomicBool,
}

#[async_trait]
impl MetadataProvider for FailsOnce {
    async fn extract(&self, _video_id: &str, _title: &str) -> Result<VideoMetadata, MetadataError> {
        if !self.failed.swap(true, Ordering::SeqCst) {
            return Err(MetadataError::ApiError("first call fails".into()));
        }
        Ok(VideoMetadata {
            song: SONG.into(),
            artist: ARTIST.into(),
            source: MetadataSource::Gemini,
            gemini_failed: false,
        })
    }

    fn name(&self) -> &str {
        "fails-once"
    }
}

#[tokio::test]
async fn every_call_is_recorded_and_an_answer_clears_the_last_error() {
    let chain = ProviderChain::new(vec![Box::new(FailsOnce {
        failed: AtomicBool::new(false),
    })]);
    let provider = &chain.providers().await.unwrap()[0];

    assert!(provider.extract(VIDEO, TITLE).await.is_err());
    let health = &chain.health()[0];
    assert_eq!(health.name, "fails-once");
    assert_eq!(
        health.last_error.as_deref(),
        Some("API request failed: first call fails")
    );
    assert_eq!(health.last_ok_at_ms, None);

    let before = chrono::Utc::now().timestamp_millis();
    assert!(provider.extract(VIDEO, TITLE).await.is_ok());
    let after = chrono::Utc::now().timestamp_millis();
    let health = &chain.health()[0];
    assert_eq!(health.last_error, None, "an answer clears the last error");
    let at = health.last_ok_at_ms.expect("the answer is recorded");
    assert!(
        (before..=after).contains(&at),
        "{before} <= {at} <= {after}"
    );
}

/// #229 item C: the production chain answers no provider while this node's
/// paid AI is off, read at every walk; a test's chain is never gated.
#[tokio::test]
async fn the_production_chain_answers_no_provider_while_paid_ai_is_off() {
    let proxy = MockServer::start().await;
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let chain = provider_chain(&pool, ai_client_at(&proxy), "k1", "gemini-test");
    crate::db::models::set_setting(&pool, "paid_ai_enabled", "false")
        .await
        .unwrap();
    assert!(chain.providers().await.is_none());
    crate::db::models::set_setting(&pool, "paid_ai_enabled", "true")
        .await
        .unwrap();
    assert_eq!(chain.providers().await.map(|p| p.len()), Some(2));
    let ungated = provider_chain_at(ai_client_at(&proxy), "k1", "gemini-test", &proxy.uri());
    crate::db::models::set_setting(&pool, "paid_ai_enabled", "false")
        .await
        .unwrap();
    assert_eq!(ungated.providers().await.map(|p| p.len()), Some(2));
}
