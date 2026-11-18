//! #144: a cleanup Claude's content filter refused is remembered for its raw
//! text (`cleanup_refusal.rs`) and never sent again until the text changes.

use super::*;
use crate::ai::client::AiClient;
use crate::ai::retry::content_filtered;
use crate::lyrics::description_provider::{CleanupMode, clean_lyrics_via_ai};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// #321: OpenRouter's moderation refusal (final, typed `ContentFiltered`).
const FILTER_BODY: &str = r#"{"error":{"code":403,"message":"Your input was flagged","metadata":{"reasons":["violence"],"flagged_input":"…"}}}"#;

#[test]
fn the_raw_text_is_known_by_its_sha256() {
    assert_eq!(
        raw_sha256("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

/// A cleanup's own answer (`{"lines": …}`) is no refusal.
#[tokio::test]
async fn a_cleanups_answer_records_no_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("yt_lrclib_cleaned_v3.json");
    std::fs::write(&path, r#"{"lines":["amazing grace"]}"#).unwrap();
    assert!(!records_a_refusal(&path).await);
    assert!(!refused_earlier(&path, "amazing grace").await);
    assert!(!records_a_refusal(&dir.path().join("none.json")).await);
}

/// The cleanup of `text` into `cache`, which must fail.
async fn refused(client: &AiClient, cache: &std::path::Path, text: &str) -> anyhow::Error {
    clean_lyrics_via_ai(client, "S", "A", text, cache, CleanupMode::ScrapedLyrics)
        .await
        .unwrap_err()
}

/// The same text is sent once: the refusal is remembered and answered from
/// the cache; a changed text is asked again (and its refusal remembered).
#[tokio::test]
async fn a_refused_cleanup_is_never_sent_again_for_the_same_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(403).set_body_string(FILTER_BODY))
        .expect(2)
        .mount(&server)
        .await;
    let client = crate::ai::test_support::ai_client_at(format!("{}/v1", server.uri())).await;
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("yt_lrclib_cleaned_v3.json");

    for _ in 0..3 {
        let err = refused(&client, &cache, "line one\nline two").await;
        assert!(content_filtered(&err), "{err:#}");
    }
    assert!(refused_earlier(&cache, "line one\nline two").await);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let err = refused(&client, &cache, "line one\nline three").await;
    assert!(content_filtered(&err), "{err:#}");
    assert!(refused_earlier(&cache, "line one\nline three").await);
    assert!(!refused_earlier(&cache, "line one\nline two").await);
}
