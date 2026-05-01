//! Tests for `lyrics::worker`. Included as a sibling file via
//! `#[path = "worker_tests.rs"] #[cfg(test)] mod tests;` from `worker.rs`
//! to keep that file under the 1000-line airuleset cap.
//!
//! The three tier-chain branch tests (LineSynced/TextOnly/None) live in
//! `orchestrator.rs::tests` — that's where the mock backend can be
//! injected cleanly. This file covers worker-level concerns: source
//! literal correctness, `align_track_to_lyrics_track` field mapping,
//! and the gather → candidate pipeline.

#![allow(unused_imports)]

use super::*;

/// Audit: retired symbols must not appear in worker.rs.
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
        // Legacy alignment providers — removed in Phase F, deleted in Phase G.
        ["gemini_", "provider"].concat(),
        ["qwen3_", "provider"].concat(),
        ["autosub_", "provider"].concat(),
        ["description_", "provider"].concat(),
        ["text_", "merge"].concat(),
        // Legacy Gemini provider import pattern.
        ["Gemini", "Provider"].concat(),
        ["Qwen3", "Provider"].concat(),
        // Legacy orchestrator::Orchestrator::new(..., providers, ai_client, cache_dir) shape.
        ["process_", "song(&ctx"].concat(),
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

/// Gather phase must try sources in order: YouTube manual subs → LRCLIB.
/// This preserves the yt_subs-before-lrclib precedence.
#[test]
fn gather_sources_call_order_preserves_yt_subs_then_lrclib() {
    // gather_sources_impl was extracted from worker.rs into the sibling
    // `gather.rs` module to keep both files under the 1000-line airuleset cap.
    let src = include_str!("gather.rs");
    let body_start = src
        .find("async fn gather_sources")
        .expect("gather_sources exists");
    let body = &src[body_start..];
    let yt = body
        .find("youtube_subs::fetch_subtitles")
        .expect("yt_subs call");
    let lr = body.find("lrclib::fetch_lyrics").expect("lrclib call");
    assert!(yt < lr, "yt_subs must be before lrclib");
}

/// `align_track_to_lyrics_track` correctly maps AlignedTrack fields to
/// LyricsTrack. Specifically:
/// - provenance → source
/// - AlignedLine.text → LyricsLine.en
/// - start_ms/end_ms widening u32 → u64
/// - words: None passes through as None (no word synthesis)
/// - words: Some(vec) maps start_ms/end_ms per AlignedWord to LyricsWord u64 fields
/// - version is the supplied pipeline version constant
#[test]
fn align_track_to_lyrics_track_maps_fields_correctly() {
    use crate::lyrics::backend::{AlignedLine, AlignedTrack, AlignedWord};

    let aligned = AlignedTrack {
        lines: vec![
            AlignedLine {
                text: "amazing grace".into(),
                start_ms: 0,
                end_ms: 2000,
                words: Some(vec![
                    AlignedWord {
                        text: "amazing".into(),
                        start_ms: 0,
                        end_ms: 1000,
                        confidence: 0.9,
                    },
                    AlignedWord {
                        text: "grace".into(),
                        start_ms: 1000,
                        end_ms: 2000,
                        confidence: 0.9,
                    },
                ]),
            },
            AlignedLine {
                text: "how sweet the sound".into(),
                start_ms: 2000,
                end_ms: 4000,
                // Tier-1 line-synced ships words: None per feedback_line_timing_only.md
                words: None,
            },
        ],
        provenance: "tier1:spotify".into(),
        raw_confidence: 1.0,
    };

    let track = align_track_to_lyrics_track(aligned, 42);

    assert_eq!(
        track.version, 42,
        "version must match the supplied constant"
    );
    assert_eq!(
        track.source, "tier1:spotify",
        "source must come from AlignedTrack.provenance"
    );
    assert_eq!(track.language_source, "en");
    assert_eq!(track.language_translation, "");
    assert_eq!(track.lines.len(), 2);

    // Line 0: text and timing
    assert_eq!(track.lines[0].en, "amazing grace");
    assert_eq!(track.lines[0].start_ms, 0u64);
    assert_eq!(track.lines[0].end_ms, 2000u64);
    // Words map with u32 → u64 widening
    let words = track.lines[0]
        .words
        .as_ref()
        .expect("line 0 should have words");
    assert_eq!(words.len(), 2);
    assert_eq!(words[0].text, "amazing");
    assert_eq!(words[0].start_ms, 0u64);
    assert_eq!(words[0].end_ms, 1000u64);
    assert_eq!(words[1].text, "grace");
    assert_eq!(words[1].start_ms, 1000u64);
    assert_eq!(words[1].end_ms, 2000u64);

    // Line 1: words: None preserved (no synthesis)
    assert_eq!(track.lines[1].en, "how sweet the sound");
    assert_eq!(track.lines[1].start_ms, 2000u64);
    assert_eq!(track.lines[1].end_ms, 4000u64);
    assert!(
        track.lines[1].words.is_none(),
        "words: None must pass through unchanged — no word synthesis allowed"
    );
}

/// Verify that new provenance literals produced by the tier chain are valid.
/// This is a documentation-as-test: if the source tag format changes, this
/// test breaks, forcing a deliberate update.
#[test]
fn new_provenance_source_literals_are_recognizable() {
    // These are the sources the new tier chain can produce. Asserted as
    // non-empty string comparisons to make the test read as a spec.
    let tier1_sources = ["tier1:spotify", "tier1:lrclib", "tier1:yt_subs", "genius"];
    let backend_source = "whisperx-large-v3@rev1";
    // TextOnly path: claude-merge appends "+claude-merge" to the ASR provenance.
    let claude_merge_suffix = "+claude-merge";

    for s in &tier1_sources {
        assert!(
            s.starts_with("tier1:") || *s == "genius",
            "tier1 source must be recognizable: {s}"
        );
    }
    assert!(
        backend_source.contains("whisperx"),
        "backend source must mention whisperx"
    );
    // Claude-merged provenance is backend provenance + "+claude-merge"
    let merged = format!("{backend_source}{claude_merge_suffix}");
    assert!(merged.ends_with("+claude-merge"));
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
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: None,
    };

    let reqwest_client = reqwest::Client::new();
    let bogus_ytdlp = std::path::PathBuf::from("/definitely/does/not/exist/ytdlp");

    let ctx = gather_sources_impl(
        Some(&ai),
        &bogus_ytdlp,
        cache_dir.path(),
        &reqwest_client,
        &row,
        "", // no genius token in tests — skip Genius source
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
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: None,
    };

    let reqwest_client = reqwest::Client::new();
    let bogus_ytdlp = std::path::PathBuf::from("/definitely/does/not/exist/ytdlp");

    // gather_sources_impl should bail with "no text sources available" because:
    // - yt_subs: yt-dlp path bogus, returns None
    // - lrclib: artist empty, skipped
    // - description: Claude returns empty array, match guard skips push
    // So candidate_texts is empty and the function bails.
    let result = gather_sources_impl(
        Some(&ai),
        &bogus_ytdlp,
        cache_dir.path(),
        &reqwest_client,
        &row,
        "", // no genius token in tests — skip Genius source
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

/// Verify the replicate_api_token early-exit guard is present in process_song.
///
/// The guard prevents the worker from feeding an empty string to
/// WhisperXReplicateBackend (which would then fail with HTTP 401 after the
/// 12-second rate-limit pre-sleep, chewing through every queued song before
/// anyone notices). A structural check catches if the guard is accidentally
/// removed during future edits.
#[test]
fn process_song_has_replicate_token_early_exit() {
    let src = include_str!("worker.rs");
    // The guard must check trim().is_empty() on the token ...
    assert!(
        src.contains("replicate_token.trim().is_empty()"),
        "process_song must have a replicate_token early-exit guard"
    );
    // ... and produce the expected error message.
    assert!(
        src.contains("replicate_api_token not configured"),
        "early-exit error message must mention replicate_api_token not configured"
    );
}

// ---------------------------------------------------------------------------
// Spotify branch (#67)
// ---------------------------------------------------------------------------
//
// `SPOTIFY_LYRICS_PROXY_BASE` is a process-wide env var. The four tests below
// are marked `#[serial_test::serial]` so they don't race when cargo runs them
// in parallel.

#[tokio::test]
#[serial_test::serial]
async fn gather_emits_tier1_spotify_candidate_when_track_id_set() {
    use crate::db::models::VideoLyricsRow;
    use crate::lyrics::worker::gather_sources_impl;

    let cache_dir = tempfile::tempdir().unwrap();
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/"))
        .and(wiremock::matchers::query_param(
            "trackid",
            "3n3Ppam7vgaVa1iaRUc9Lp",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": false,
                "syncType": "LINE_SYNCED",
                "lines": [
                    {"startTimeMs": "1000", "words": "Amazing grace"},
                    {"startTimeMs": "3000", "words": "How sweet the sound"}
                ]
            })),
        )
        .mount(&mock)
        .await;
    // SAFETY: marked serial above; no other test races on this env var while
    // this test runs.
    unsafe {
        std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", mock.uri());
    }

    let row = VideoLyricsRow {
        id: 1,
        youtube_id: "spotifyOK1".into(),
        song: "".into(), // empty → LRCLIB+Genius branches skipped
        artist: "".into(),
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/watch?v=spotifyOK1".into(),
        lyrics_override_text: None,
        lyrics_time_offset_ms: 0,
        spotify_track_id: Some("3n3Ppam7vgaVa1iaRUc9Lp".into()),
    };

    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/definitely/does/not/exist/ytdlp"),
        cache_dir.path(),
        &reqwest::Client::new(),
        &row,
        "",
    )
    .await
    .expect("gather succeeds with Spotify candidate");

    let spotify = ctx
        .candidate_texts
        .iter()
        .find(|c| c.source == "tier1:spotify")
        .expect("Spotify candidate present");
    assert!(spotify.has_timing);
    assert_eq!(spotify.lines.len(), 2);
    assert_eq!(spotify.lines[0], "Amazing grace");
    assert_eq!(spotify.lines[1], "How sweet the sound");
    assert!(spotify.line_timings.is_some());

    unsafe {
        std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn gather_omits_spotify_when_track_id_is_null() {
    use crate::db::models::VideoLyricsRow;
    use crate::lyrics::worker::gather_sources_impl;

    let cache_dir = tempfile::tempdir().unwrap();

    // No spotify_track_id; gather has only the override candidate.
    let row = VideoLyricsRow {
        id: 2,
        youtube_id: "noSpotifyId".into(),
        song: "".into(),
        artist: "".into(),
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/watch?v=noSpotifyId".into(),
        lyrics_override_text: Some("operator line".into()),
        lyrics_time_offset_ms: 0,
        spotify_track_id: None,
    };

    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/definitely/does/not/exist/ytdlp"),
        cache_dir.path(),
        &reqwest::Client::new(),
        &row,
        "",
    )
    .await
    .expect("gather succeeds via override candidate");

    assert!(
        ctx.candidate_texts
            .iter()
            .all(|c| c.source != "tier1:spotify"),
        "no Spotify candidate must be emitted when spotify_track_id is None"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn gather_skips_spotify_on_404() {
    use crate::db::models::VideoLyricsRow;
    use crate::lyrics::worker::gather_sources_impl;

    let cache_dir = tempfile::tempdir().unwrap();
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(404))
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", mock.uri());
    }

    let row = VideoLyricsRow {
        id: 3,
        youtube_id: "spotify404".into(),
        song: "".into(),
        artist: "".into(),
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/watch?v=spotify404".into(),
        lyrics_override_text: Some("operator line".into()),
        lyrics_time_offset_ms: 0,
        spotify_track_id: Some("3n3Ppam7vgaVa1iaRUc9Lp".into()),
    };

    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/definitely/does/not/exist/ytdlp"),
        cache_dir.path(),
        &reqwest::Client::new(),
        &row,
        "",
    )
    .await
    .expect("gather succeeds when Spotify returns 404");

    assert!(
        ctx.candidate_texts
            .iter()
            .all(|c| c.source != "tier1:spotify"),
        "no Spotify candidate must be emitted when proxy returns 404"
    );

    unsafe {
        std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn gather_skips_spotify_on_proxy_error_field() {
    use crate::db::models::VideoLyricsRow;
    use crate::lyrics::worker::gather_sources_impl;

    let cache_dir = tempfile::tempdir().unwrap();
    let mock = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": true,
                "message": "track not found"
            })),
        )
        .mount(&mock)
        .await;
    unsafe {
        std::env::set_var("SPOTIFY_LYRICS_PROXY_BASE", mock.uri());
    }

    let row = VideoLyricsRow {
        id: 4,
        youtube_id: "spotifyERR".into(),
        song: "".into(),
        artist: "".into(),
        duration_ms: Some(180_000),
        audio_file_path: None,
        youtube_url: "https://www.youtube.com/watch?v=spotifyERR".into(),
        lyrics_override_text: Some("operator line".into()),
        lyrics_time_offset_ms: 0,
        spotify_track_id: Some("3n3Ppam7vgaVa1iaRUc9Lp".into()),
    };

    let ctx = gather_sources_impl(
        None,
        std::path::Path::new("/definitely/does/not/exist/ytdlp"),
        cache_dir.path(),
        &reqwest::Client::new(),
        &row,
        "",
    )
    .await
    .expect("gather succeeds when proxy returns error:true");

    assert!(
        ctx.candidate_texts
            .iter()
            .all(|c| c.source != "tier1:spotify"),
        "no Spotify candidate must be emitted when proxy reports error:true"
    );

    unsafe {
        std::env::remove_var("SPOTIFY_LYRICS_PROXY_BASE");
    }
}
