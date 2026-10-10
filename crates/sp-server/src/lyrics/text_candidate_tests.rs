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
    let cache = dir.path().join("yt1_title_genius_x_cleaned_v3.json");

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
    let cache = dir.path().join("yt1_title_lrclib_7_cleaned_v3.json");
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
    let cache = dir.path().join("yt1_title_lrclib_8_cleaned_v3.json");
    tokio::fs::write(&cache, r#"{"lines": []}"#).await.unwrap();
    let ai = ai_at("http://127.0.0.1:9/v1".to_string());
    let t = track(&[(0, 0, "Something")]);
    let got = cleaned_text_candidate(&ai, "Song", "Artist", "lrclib", &t, &cache)
        .await
        .unwrap();
    assert_eq!(got.map(|c| c.lines), None);
}

fn cleaned() -> CandidateText {
    CandidateText {
        source: "genius".into(),
        lines: vec!["amazing grace".into()],
        has_timing: false,
        line_timings: None,
    }
}

/// The error a cleanup refused by Claude's upstream content filter carries
/// up to `gather`, as `AiClient::chat` and `clean_lyrics_via_claude` build it
/// (SNV log 10.10.2026).
fn content_filtered() -> anyhow::Error {
    anyhow::anyhow!(
        "chat completion failed (HTTP 502 Bad Gateway): {{\"error\":{{\"message\":\"claude \
         executor: upstream returned error event: Output blocked by content filtering \
         policy\",\"type\":\"server_error\"}}}}"
    )
    .context("Claude clean_lyrics chat failed")
}

/// #144: a cleanup Claude's upstream content filter refuses leaves out THAT
/// candidate — the pass goes on with the other sources. It failed the whole
/// pass before, so h-A1Tzkjsi4, ejDeQIA677g and UU9ctYCtkFk left the
/// reprocess queue after 3 attempts without a pass (SNV, 10.10.2026).
#[test]
fn a_cleanup_the_content_filter_refuses_leaves_out_only_its_candidate() {
    let fate = cleanup_candidate(Err(content_filtered()), "genius fallback", "yt1");
    assert!(matches!(fate, Ok(None)), "{fate:?}");
}

/// #144: any other failure (an outage) still fails the pass, which the
/// backoff waits out; a cleanup that finds no lyric fails it too.
#[test]
fn a_failed_or_empty_cleanup_still_fails_the_pass() {
    let outage = cleanup_candidate(
        Err(anyhow::anyhow!(
            "chat completion failed (HTTP 503): auth_unavailable"
        )),
        "genius fallback",
        "yt1",
    )
    .unwrap_err()
    .to_string();
    assert!(
        outage.contains("gather: genius fallback cleanup failed for yt1"),
        "{outage}"
    );
    let empty = cleanup_candidate(Ok(None), "lrclib-plain", "yt2")
        .unwrap_err()
        .to_string();
    assert_eq!(
        empty,
        "gather: lrclib-plain cleanup returned no lyrics for yt2"
    );
    let kept = cleanup_candidate(Ok(Some(cleaned())), "genius fallback", "yt1").unwrap();
    assert_eq!(
        kept.map(|c| c.lines),
        Some(vec!["amazing grace".to_string()])
    );
}
