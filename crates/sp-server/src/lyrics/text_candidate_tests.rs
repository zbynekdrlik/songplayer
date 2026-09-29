//! Tests for `text_candidate.rs` (#144).

use super::*;
use crate::ai::AiSettings;
use crate::ai::client::AiClient;
use sp_core::lyrics::{LyricsLine, LyricsTrack};

fn track(lines: &[(u64, u64, &str)]) -> LyricsTrack {
    LyricsTrack {
        version: 1,
        source: "lrclib".into(),
        language_source: "en".into(),
        language_translation: String::new(),
        lines: lines
            .iter()
            .map(|&(start_ms, end_ms, en)| LyricsLine {
                start_ms,
                end_ms,
                en: en.to_string(),
                sk: None,
                words: None,
            })
            .collect(),
    }
}

fn ai_at(url: String) -> AiClient {
    AiClient::new(AiSettings {
        api_url: url,
        api_key: Some("test".into()),
        model: "claude-test".into(),
        system_prompt_extra: None,
    })
}

#[test]
fn a_timed_track_becomes_a_timed_candidate_with_its_own_timings() {
    let t = track(&[(1_500, 4_200, "Amazing grace"), (4_200, 7_000, "How sweet")]);
    let c = timed_candidate("lrclib", &t);
    assert_eq!(c.source, "lrclib");
    assert_eq!(c.lines, vec!["Amazing grace", "How sweet"]);
    assert!(c.has_timing);
    assert_eq!(c.line_timings, Some(vec![(1_500, 4_200), (4_200, 7_000)]));
}

/// Claude cleans the scraped text; the cleaned lines become a text-only
/// candidate under the given source, and the decision is cached.
#[tokio::test]
async fn a_scraped_text_is_cleaned_into_a_text_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "{\"lines\": [\"You never fail\", \"You never will\"]}"
                    }
                }]
            })),
        )
        .expect(1)
        .mount(&mock)
        .await;
    let ai = ai_at(format!("{}/v1", mock.uri()));
    let t = track(&[
        (0, 0, "[Chorus]"),
        (0, 0, "You never fail"),
        (0, 0, "You never will"),
    ]);
    let cache = dir.path().join("yt1_title_genius_x_cleaned_v2.json");

    let c = cleaned_text_candidate(&ai, "Jesus Be the Name", "", "genius", &t, &cache)
        .await
        .unwrap()
        .expect("Claude returned lyric lines");
    assert_eq!(c.source, "genius");
    assert_eq!(c.lines, vec!["You never fail", "You never will"]);
    assert!(!c.has_timing);
    assert_eq!(c.line_timings, None);

    let cached = tokio::fs::read_to_string(&cache).await.unwrap();
    assert!(cached.contains("You never will"), "cached: {cached}");
}

/// A cleanup that finds no lyric (`{"lines": null}` cached) gives no
/// candidate — and a cached decision is reused with no Claude call (the
/// client points nowhere).
#[tokio::test]
async fn a_text_the_cleanup_rejects_gives_no_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("yt1_title_lrclib_7_cleaned_v2.json");
    tokio::fs::write(&cache, r#"{"lines": null}"#)
        .await
        .unwrap();
    let ai = ai_at("http://127.0.0.1:9/v1".to_string());
    let t = track(&[(0, 0, "Buy my album")]);
    let got = cleaned_text_candidate(&ai, "Song", "Artist", "lrclib", &t, &cache)
        .await
        .unwrap();
    assert_eq!(got.map(|c| c.lines), None);
}

/// An EMPTY cleaned list is no candidate either (a candidate with no lines
/// would outrank nothing and align nothing).
#[tokio::test]
async fn an_empty_cleanup_gives_no_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("yt1_title_lrclib_8_cleaned_v2.json");
    tokio::fs::write(&cache, r#"{"lines": []}"#).await.unwrap();
    let ai = ai_at("http://127.0.0.1:9/v1".to_string());
    let t = track(&[(0, 0, "Something")]);
    let got = cleaned_text_candidate(&ai, "Song", "Artist", "lrclib", &t, &cache)
        .await
        .unwrap();
    assert_eq!(got.map(|c| c.lines), None);
}
