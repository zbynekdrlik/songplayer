//! Tests for `lyrics::worker`. Included as a sibling file via
//! `#[path = "worker_tests.rs"] #[cfg(test)] mod tests;` from `worker.rs`
//! to keep that file under the 1000-line airuleset cap.

#![allow(unused_imports)]

use super::*;

/// Audit: retired symbols must not appear in this file.
///
/// NOTE: banned symbol names are split across two string literals joined
/// at runtime so this test file does not contain the verbatim string it is
/// checking for (which would cause the test to always fail on itself).
#[test]
fn worker_has_no_retired_symbols() {
    let src = include_str!("worker.rs");
    let banned = [
        ["retry_missing", "_alignment"].concat(),
        ["count_duplicate", "_start_ms"].concat(),
        ["merge_word", "_timings"].concat(),
        ["ensure_progressive", "_words"].concat(),
        ["set_video", "_lyrics_source"].concat(),
        ["get_next_video_missing", "_alignment"].concat(),
        ["acquire_", "lyrics"].concat(),
        ["run_chunked_", "alignment"].concat(),
        ["warn_on_degenerate_", "lines"].concat(),
    ];
    for sym in &banned {
        assert!(
            !src.contains(sym.as_str()),
            "worker.rs must not contain retired symbol `{sym}`"
        );
    }
    // The retired lyrics_source value must not appear as a literal.
    // Split to avoid self-match.
    let retired_source = ["\"lrclib", "+qwen3\""].concat();
    assert!(
        !src.contains(retired_source.as_str()),
        "worker.rs must not write the retired 'lrclib+qwen3' source literal"
    );
}

/// Gather phase must try sources in order: YouTube manual subs → LRCLIB → autosub.
/// This preserves the legacy yt_subs-before-lrclib precedence and puts the cheapest
/// miss (autosub) last.
#[test]
fn gather_sources_call_order_preserves_yt_subs_then_lrclib_then_autosub() {
    let src = include_str!("worker.rs");
    let body_start = src
        .find("async fn gather_sources")
        .expect("gather_sources exists");
    let body = &src[body_start..];
    let yt = body
        .find("youtube_subs::fetch_subtitles")
        .expect("yt_subs call");
    let lr = body.find("lrclib::fetch_lyrics").expect("lrclib call");
    let au = body.find("fetch_autosub(").expect("autosub call");
    assert!(yt < lr, "yt_subs must be before lrclib");
    assert!(lr < au, "lrclib must be before autosub");
}

/// Description provider is wired as the 4th candidate source.
/// Seeds a raw-description cache file so yt-dlp is never invoked,
/// stubs Claude via wiremock, and asserts the returned SongContext
/// contains exactly one CandidateText with source == "description".
#[tokio::test]
async fn gather_sources_pushes_description_candidate_when_claude_returns_lyrics() {
    use crate::ai::AiSettings;
    use crate::ai::client::AiClient;
    use crate::db::models::VideoLyricsRow;
    use crate::lyrics::worker::gather_sources_impl;

    let cache_dir = tempfile::tempdir().unwrap();

    // Pre-seed the raw description cache so yt-dlp is never invoked.
    tokio::fs::write(
        cache_dir.path().join("vidDESC_description.txt"),
        "Lyrics:\nAmazing grace\nHow sweet the sound",
    )
    .await
    .unwrap();

    // Stub Claude: return a valid JSON lyrics response.
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "{\"lines\": [\"Amazing grace\", \"How sweet the sound\"]}"
                    }
                }]
            })),
        )
        .mount(&mock)
        .await;

    let ai = AiClient::new(AiSettings {
        api_url: format!("{}/v1", mock.uri()),
        api_key: Some("test".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });

    let row = VideoLyricsRow {
        id: 1,
        youtube_id: "vidDESC".into(),
        song: "Amazing Grace".into(),
        artist: "".into(), // empty artist → lrclib skipped
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/watch?v=vidDESC".into(),
    };

    let autosub_tmp = tempfile::tempdir().unwrap();
    let reqwest_client = reqwest::Client::new();
    let bogus_ytdlp = std::path::PathBuf::from("/definitely/does/not/exist/ytdlp");

    let ctx = gather_sources_impl(
        Some(&ai),
        &bogus_ytdlp,
        cache_dir.path(),
        &reqwest_client,
        &row,
        autosub_tmp.path(),
    )
    .await
    .unwrap();

    assert_eq!(
        ctx.candidate_texts.len(),
        1,
        "only description should be present; got: {:?}",
        ctx.candidate_texts
            .iter()
            .map(|c| &c.source)
            .collect::<Vec<_>>()
    );
    assert_eq!(ctx.candidate_texts[0].source, "description");
    assert_eq!(
        ctx.candidate_texts[0].lines,
        vec![
            "Amazing grace".to_string(),
            "How sweet the sound".to_string()
        ]
    );
    assert!(!ctx.candidate_texts[0].has_timing);
}

/// Regression: when Claude returns `{"lines": []}` for a song that has no
/// lyrics in its description, the description block must NOT push an empty
/// CandidateText. The match guard `!lines.is_empty()` prevents that. Replacing
/// the guard with `true` would allow an empty description candidate through,
/// but then candidate_texts would not be empty and the function would not bail.
/// This test verifies that empty-array responses are correctly skipped.
#[tokio::test]
async fn gather_sources_skips_description_when_claude_returns_empty_array() {
    use crate::ai::AiSettings;
    use crate::ai::client::AiClient;
    use crate::db::models::VideoLyricsRow;
    use crate::lyrics::worker::gather_sources_impl;

    let cache_dir = tempfile::tempdir().unwrap();
    tokio::fs::write(
        cache_dir.path().join("vidEMPTY2_description.txt"),
        "some description with no actual lyrics",
    )
    .await
    .unwrap();

    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "{\"lines\": []}"
                    }
                }]
            })),
        )
        .mount(&mock)
        .await;
    let ai = AiClient::new(AiSettings {
        api_url: format!("{}/v1", mock.uri()),
        api_key: Some("test".into()),
        model: "stub".into(),
        system_prompt_extra: None,
    });

    let row = VideoLyricsRow {
        id: 2,
        youtube_id: "vidEMPTY2".into(),
        song: "Something".into(),
        artist: "".into(), // empty -> lrclib skipped
        duration_ms: Some(120_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/watch?v=vidEMPTY2".into(),
    };

    let autosub_tmp = tempfile::tempdir().unwrap();
    let reqwest_client = reqwest::Client::new();
    let bogus_ytdlp = std::path::PathBuf::from("/definitely/does/not/exist/ytdlp");

    // gather_sources_impl should bail with "no text sources available" because:
    // - yt_subs: yt-dlp path bogus, returns None
    // - lrclib: artist empty, skipped
    // - autosub: bogus path, returns None
    // - description: Claude returns empty array, match guard skips push
    // So candidate_texts is empty and the function bails.
    let result = gather_sources_impl(
        Some(&ai),
        &bogus_ytdlp,
        cache_dir.path(),
        &reqwest_client,
        &row,
        autosub_tmp.path(),
    )
    .await;

    assert!(
        result.is_err(),
        "expected bail on zero candidates, got: {:?}",
        result
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("no text sources available"),
        "expected 'no text sources available' error, got: {err_msg}"
    );
}
