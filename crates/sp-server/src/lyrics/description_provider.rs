//! YouTube description lyrics provider.
//!
//! Fetches the raw description via yt-dlp, pipes it through a narrow Claude
//! prompt, and emits a `CandidateText { source: "description" }` for the
//! ensemble text-merge step. Caches both the raw description and the
//! extracted lyrics JSON on disk so reprocesses reuse the work.

use anyhow::{Context, Result};
use std::path::Path;
use tracing::{debug, warn};

use crate::ai::client::AiClient;

// Prompts split into a sibling module (description_provider_prompts.rs) so
// this file stays under the 1000-line cap. Re-export the public surface
// (`CleanupMode`, `build_description_extraction_prompt`,
// `build_scraped_lyrics_cleanup_prompt`) so external callers stay unchanged.
#[path = "description_provider_prompts.rs"]
mod prompts;
pub use prompts::{
    CleanupMode, build_description_extraction_prompt, build_scraped_lyrics_cleanup_prompt,
};

/// Parse Claude's response to the description extraction prompt.
///
/// Handles three cases:
/// - `{"lines": [...]}` → `Ok(Some(vec))`
/// - `{"lines": null}` → `Ok(None)`
/// - Markdown fences or preamble → strips via `crate::ai::client::strip_markdown_fences` before parsing
/// - Malformed JSON / missing "lines" key / wrong type → `Err`
pub(crate) fn parse_claude_response(raw: &str) -> Result<Option<Vec<String>>> {
    let cleaned = crate::ai::client::strip_markdown_fences(raw);
    let v: serde_json::Value = serde_json::from_str(&cleaned)
        .with_context(|| format!("failed to parse Claude response as JSON: {cleaned}"))?;
    let lines = v
        .get("lines")
        .ok_or_else(|| anyhow::anyhow!("missing 'lines' key in Claude response: {cleaned}"))?;
    if lines.is_null() {
        return Ok(None);
    }
    let arr = lines
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("'lines' is not an array or null: {cleaned}"))?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if out.len() != arr.len() {
        anyhow::bail!("some elements of 'lines' were not strings: {cleaned}");
    }
    Ok(Some(out))
}

/// Read the cached extracted-lyrics JSON.
///
/// Returns:
/// - `Ok(None)` when the file does not exist (no cache yet).
/// - `Ok(Some(None))` when the cache records that this song has no lyrics in its description.
/// - `Ok(Some(Some(lines)))` when the cache has extracted lyric lines.
/// - `Err` when the file exists but is malformed (we refuse to silently discard it).
pub(crate) async fn read_lyrics_cache(path: &Path) -> Result<Option<Option<Vec<String>>>> {
    let Ok(bytes) = tokio::fs::read(path).await else {
        return Ok(None);
    };
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).context("malformed description_lyrics cache")?;
    let lines = v
        .get("lines")
        .ok_or_else(|| anyhow::anyhow!("cache missing 'lines' key"))?;
    if lines.is_null() {
        return Ok(Some(None));
    }
    let arr = lines
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("cache 'lines' is not array or null"))?;
    let out: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if out.len() != arr.len() {
        anyhow::bail!("cache 'lines' contains non-string elements");
    }
    Ok(Some(Some(out)))
}

/// Write the extracted-lyrics JSON cache.
///
/// `lines = Some(&[...])` writes `{"lines": [...]}`.
/// `lines = None` writes `{"lines": null}`.
pub(crate) async fn write_lyrics_cache(path: &Path, lines: Option<&[String]>) -> Result<()> {
    let body = match lines {
        Some(l) => serde_json::json!({ "lines": l }),
        None => serde_json::json!({ "lines": null }),
    };
    let s = serde_json::to_string(&body)?;
    tokio::fs::write(path, s).await.context("write cache")?;
    Ok(())
}

/// Fetch the YouTube video description, using a disk cache keyed by `youtube_id`.
///
/// Returns `Ok(Some(text))` when the description is available (cached or freshly
/// fetched), `Ok(None)` when yt-dlp failed and no cache exists. Never creates a
/// cache file on failure — so the next reprocess retries.
// mutants::skip: subprocess I/O wrapper; behaviour covered by cached-hit test and
// subprocess-failure integration test.
#[cfg_attr(test, mutants::skip)]
pub(crate) async fn fetch_raw_description(
    ytdlp_path: &Path,
    youtube_id: &str,
    cache_dir: &Path,
) -> Result<Option<String>> {
    let cache_path = cache_dir.join(format!("{youtube_id}_description.txt"));
    if let Ok(cached) = tokio::fs::read_to_string(&cache_path).await {
        debug!(
            youtube_id,
            "description_provider: cache hit (raw description)"
        );
        return Ok(Some(cached));
    }

    let url = format!("https://www.youtube.com/watch?v={youtube_id}");
    let mut cmd = tokio::process::Command::new(ytdlp_path);
    cmd.arg("--skip-download")
        .arg("--no-warnings")
        .arg("--print")
        .arg("%(description)s")
        .arg(&url);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.kill_on_drop(true);

    let output = match cmd.output().await {
        Ok(o) => o,
        Err(e) => {
            warn!(youtube_id, %e, "description_provider: yt-dlp spawn failed; skipping");
            return Ok(None);
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            youtube_id,
            status = ?output.status,
            stderr = %stderr,
            "description_provider: yt-dlp returned non-zero; skipping"
        );
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        debug!(
            youtube_id,
            "description_provider: yt-dlp returned empty description"
        );
        // Cache the empty result so we don't re-spawn yt-dlp on reprocess.
        let _ = tokio::fs::write(&cache_path, "").await;
        return Ok(Some(String::new()));
    }
    tokio::fs::write(&cache_path, &text)
        .await
        .context("write description cache")?;
    Ok(Some(text))
}

/// Claude-clean a raw lyrics blob. Reuses the description-extraction prompt
/// (`build_description_extraction_prompt`). Reads / writes the cleaned-lines
/// cache at `cache_path`.
///
/// Returns:
/// - `Ok(Some(lines))` — Claude produced lines (may be empty; callers must guard `!lines.is_empty()`).
/// - `Ok(None)`        — Claude returned `{"lines": null}` (refusal / no lyrics).
/// - `Err(_)`          — transport error, malformed JSON, or IO error.
///
/// Caller policy decides whether `Ok(None)` is fatal:
/// - description: `Ok(None)` is normal (description had no lyrics).
/// - genius / lrclib-plain: `Ok(None)` is fatal (caller must bail).
///
/// Cache contract matches `write_lyrics_cache`: on success the result is
/// persisted so subsequent reprocesses skip Claude. On `Err`, no cache is
/// written so the next attempt retries.
pub async fn clean_lyrics_via_claude(
    ai: &AiClient,
    title: &str,
    artist: &str,
    raw_blob: &str,
    cache_path: &Path,
    mode: CleanupMode,
) -> Result<Option<Vec<String>>> {
    // Fast path: cache already records a decision.
    if let Some(cached) = read_lyrics_cache(cache_path).await? {
        debug!(
            cache_path = %cache_path.display(),
            "clean_lyrics_via_claude: cache hit"
        );
        return Ok(cached);
    }

    let (system, user) = match mode {
        CleanupMode::Description => build_description_extraction_prompt(title, artist, raw_blob),
        CleanupMode::ScrapedLyrics => build_scraped_lyrics_cleanup_prompt(title, artist, raw_blob),
    };
    let raw = ai
        .chat_with_timeout(&system, &user, 180)
        .await
        .context("Claude clean_lyrics chat failed")?;
    let parsed = parse_claude_response(&raw).context("Claude response malformed")?;

    write_lyrics_cache(cache_path, parsed.as_deref()).await?;
    Ok(parsed)
}

/// Fetch and extract lyrics from a YouTube video description.
///
/// Caches both the raw description and the extracted lyrics JSON per
/// `youtube_id`, so subsequent calls short-circuit any yt-dlp or Claude
/// work. `Ok(None)` means no lyrics available; `Ok(Some(lines))` means
/// the caller should push a `CandidateText { source: "description" }`.
// mutants::skip: orchestration across yt-dlp + Claude I/O; behaviour covered by
// cached-hit, no-lyrics, success, malformed-response, and ytdlp-failure tests.
#[cfg_attr(test, mutants::skip)]
pub async fn fetch_description_lyrics(
    ai: &AiClient,
    ytdlp_path: &Path,
    youtube_id: &str,
    cache_dir: &Path,
    title: &str,
    artist: &str,
) -> Result<Option<Vec<String>>> {
    let lyrics_cache_path = cache_dir.join(format!("{youtube_id}_description_lyrics.json"));

    // Raw description fetch (cached separately).
    let Some(description) = fetch_raw_description(ytdlp_path, youtube_id, cache_dir).await? else {
        return Ok(None);
    };
    if description.trim().is_empty() {
        // Description was genuinely empty — record "no lyrics" so next
        // reprocess skips instantly.
        write_lyrics_cache(&lyrics_cache_path, None).await?;
        return Ok(None);
    }

    // Delegate Claude call + cache write to the shared helper.
    // description-specific policy: swallow Claude errors to Ok(None) so other
    // gather sources (yt_subs / lrclib / genius) still get a chance. The
    // shared helper does NOT write the cache on Err, so the next reprocess
    // retries cleanly.
    match clean_lyrics_via_claude(
        ai,
        title,
        artist,
        &description,
        &lyrics_cache_path,
        CleanupMode::Description,
    )
    .await
    {
        Ok(parsed) => Ok(parsed),
        Err(e) => {
            warn!(youtube_id, %e, "description_provider: Claude cleanup failed");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_has_rule_about_null_when_no_lyrics() {
        let (_system, user) =
            build_description_extraction_prompt("Song", "Artist", "some description");
        assert!(
            user.contains("null"),
            "user prompt must mention the null case: {user}"
        );
        assert!(
            user.contains("\"lines\""),
            "user prompt must name the JSON key: {user}"
        );
    }

    #[test]
    fn prompt_includes_title_artist_and_description_in_user_message() {
        let (_system, user) = build_description_extraction_prompt(
            "How Great Thou Art",
            "Planetshakers",
            "Here are the lyrics:\nHow great thou art",
        );
        assert!(user.contains("How Great Thou Art"), "title missing: {user}");
        assert!(user.contains("Planetshakers"), "artist missing: {user}");
        assert!(
            user.contains("How great thou art"),
            "description body missing: {user}"
        );
    }

    #[test]
    fn prompt_forbids_fabrication() {
        let (_system, user) = build_description_extraction_prompt("S", "A", "desc");
        assert!(
            user.contains("fabricate") || user.contains("not confident"),
            "user prompt must warn against fabrication: {user}"
        );
    }

    #[test]
    fn prompt_requires_original_language() {
        let (_system, user) = build_description_extraction_prompt("S", "A", "desc");
        assert!(
            user.contains("Preserve") && user.contains("translate"),
            "user prompt must require original-language preservation: {user}"
        );
    }

    #[test]
    fn prompt_uses_software_engineering_framing() {
        // Regression test: this is the crux of why the prompt works. If someone
        // removes the "building a karaoke app" framing, Claude will revert to
        // conversational mode and the whole provider stops producing JSON.
        let (system, user) = build_description_extraction_prompt("S", "A", "desc");
        assert_eq!(
            system, "",
            "system prompt must be empty (soft-framing in user)"
        );
        assert!(
            user.to_lowercase().contains("karaoke") && user.to_lowercase().contains("church"),
            "user prompt must use software-engineering framing about a karaoke app for a church: {user}"
        );
    }

    #[test]
    fn parse_lines_array_returns_some() {
        let raw = r#"{"lines": ["How great thou art", "O Lord my God"]}"#;
        let out = parse_claude_response(raw).unwrap();
        assert_eq!(
            out,
            Some(vec![
                "How great thou art".to_string(),
                "O Lord my God".to_string(),
            ])
        );
    }

    #[test]
    fn parse_lines_null_returns_none() {
        let raw = r#"{"lines": null}"#;
        let out = parse_claude_response(raw).unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn parse_handles_markdown_fences() {
        let raw = "```json\n{\"lines\": [\"line one\"]}\n```";
        let out = parse_claude_response(raw).unwrap();
        assert_eq!(out, Some(vec!["line one".to_string()]));
    }

    #[test]
    fn parse_handles_preamble_before_fences() {
        let raw = "I'll analyze the description.\n```json\n{\"lines\":null}\n```";
        let out = parse_claude_response(raw).unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(parse_claude_response("not json at all").is_err());
        assert!(parse_claude_response("{ not json").is_err());
    }

    #[test]
    fn parse_rejects_missing_lines_key() {
        assert!(parse_claude_response(r#"{"foo": []}"#).is_err());
    }

    #[test]
    fn parse_rejects_wrong_lines_type() {
        assert!(parse_claude_response(r#"{"lines": "string not array"}"#).is_err());
        assert!(parse_claude_response(r#"{"lines": 42}"#).is_err());
    }

    #[tokio::test]
    async fn cache_roundtrip_with_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc_description_lyrics.json");
        write_lyrics_cache(&p, Some(&["one".into(), "two".into()]))
            .await
            .unwrap();
        let back = read_lyrics_cache(&p).await.unwrap();
        assert_eq!(back, Some(Some(vec!["one".into(), "two".into()])));
    }

    #[tokio::test]
    async fn cache_roundtrip_with_null() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("abc_description_lyrics.json");
        write_lyrics_cache(&p, None).await.unwrap();
        let back = read_lyrics_cache(&p).await.unwrap();
        assert_eq!(back, Some(None));
    }

    #[tokio::test]
    async fn cache_missing_file_returns_ok_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nonexistent_description_lyrics.json");
        let back = read_lyrics_cache(&p).await.unwrap();
        assert_eq!(back, None);
    }

    #[tokio::test]
    async fn fetch_raw_description_returns_cached_without_subprocess() {
        let dir = tempfile::tempdir().unwrap();
        let cached_path = dir.path().join("videoid_description.txt");
        tokio::fs::write(&cached_path, "hello from cache")
            .await
            .unwrap();

        // If the function tries to spawn the bogus ytdlp path below, the test
        // fails. Cached-read path must short-circuit.
        let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");
        let out = fetch_raw_description(bogus_ytdlp, "videoid", dir.path())
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("hello from cache"));
    }

    #[tokio::test]
    async fn fetch_raw_description_returns_none_when_ytdlp_missing_and_no_cache() {
        let dir = tempfile::tempdir().unwrap();
        let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");
        let out = fetch_raw_description(bogus_ytdlp, "novideo", dir.path()).await;
        // Subprocess spawn failure returns Ok(None) — description is optional.
        assert!(matches!(out, Ok(None)));
        // No cache file should have been written on failure.
        assert!(!dir.path().join("novideo_description.txt").exists());
    }

    use crate::ai::AiSettings;

    #[tokio::test]
    async fn fetch_description_lyrics_returns_cached_lines_with_no_claude_call() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-seed both cache files.
        tokio::fs::write(
            dir.path().join("vid123_description.txt"),
            "[Verse 1]\nAmazing grace how sweet the sound",
        )
        .await
        .unwrap();
        write_lyrics_cache(
            &dir.path().join("vid123_description_lyrics.json"),
            Some(&["Amazing grace".into(), "how sweet the sound".into()]),
        )
        .await
        .unwrap();

        // AiClient pointed at an unreachable URL. If the code calls Claude, the
        // test hangs/errors and we'd notice.
        let ai = AiClient::new(AiSettings {
            api_url: "http://127.0.0.1:1/v1".into(),
            api_key: Some("never-used".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });

        let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");
        let out = fetch_description_lyrics(
            &ai,
            bogus_ytdlp,
            "vid123",
            dir.path(),
            "Amazing Grace",
            "Chris Tomlin",
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            Some(vec!["Amazing grace".into(), "how sweet the sound".into(),])
        );
    }

    #[tokio::test]
    async fn fetch_description_lyrics_returns_none_when_cache_records_no_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("vidNOLYR_description.txt"),
            "just promo text",
        )
        .await
        .unwrap();
        write_lyrics_cache(&dir.path().join("vidNOLYR_description_lyrics.json"), None)
            .await
            .unwrap();

        let ai = AiClient::new(AiSettings {
            api_url: "http://127.0.0.1:1/v1".into(),
            api_key: Some("never-used".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });
        let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

        let out =
            fetch_description_lyrics(&ai, bogus_ytdlp, "vidNOLYR", dir.path(), "Song", "Artist")
                .await
                .unwrap();
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn fetch_description_lyrics_calls_claude_and_caches_result() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-seed ONLY the raw description so yt-dlp is skipped.
        tokio::fs::write(
            dir.path().join("vidCALL_description.txt"),
            "[Verse]\nFull lyrics below:\nLine A\nLine B",
        )
        .await
        .unwrap();

        // wiremock stubs the Claude endpoint.
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "{\"lines\": [\"Line A\", \"Line B\"]}"
                        }
                    }]
                })),
            )
            .mount(&mock)
            .await;

        let ai = AiClient::new(AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "claude-opus-4-20250514".into(),
            system_prompt_extra: None,
        });
        let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

        let out =
            fetch_description_lyrics(&ai, bogus_ytdlp, "vidCALL", dir.path(), "Song", "Artist")
                .await
                .unwrap();
        assert_eq!(out, Some(vec!["Line A".into(), "Line B".into()]));

        // Cache should now contain the parsed result.
        let cache = read_lyrics_cache(&dir.path().join("vidCALL_description_lyrics.json"))
            .await
            .unwrap();
        assert_eq!(cache, Some(Some(vec!["Line A".into(), "Line B".into()])));
    }

    #[tokio::test]
    async fn fetch_description_lyrics_caches_null_when_claude_says_no_lyrics() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("vidNULL_description.txt"),
            "Buy my album! Subscribe! Links below.",
        )
        .await
        .unwrap();

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "{\"lines\": null}"}
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
        let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

        let out =
            fetch_description_lyrics(&ai, bogus_ytdlp, "vidNULL", dir.path(), "Song", "Artist")
                .await
                .unwrap();
        assert_eq!(out, None);

        let cache = read_lyrics_cache(&dir.path().join("vidNULL_description_lyrics.json"))
            .await
            .unwrap();
        assert_eq!(
            cache,
            Some(None),
            "null result must be cached for instant reprocess"
        );
    }

    #[tokio::test]
    async fn fetch_description_lyrics_no_cache_on_malformed_claude_response() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("vidBAD_description.txt"),
            "some description",
        )
        .await
        .unwrap();

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "this is not JSON at all"}
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
        let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

        let out = fetch_description_lyrics(&ai, bogus_ytdlp, "vidBAD", dir.path(), "S", "A")
            .await
            .unwrap();
        assert_eq!(out, None);
        // CRITICAL: no cache file on malformed response — we MUST retry on next reprocess.
        assert!(
            !dir.path().join("vidBAD_description_lyrics.json").exists(),
            "malformed Claude response must NOT write a cache entry"
        );
    }

    #[tokio::test]
    async fn fetch_description_lyrics_empty_raw_caches_null_no_claude() {
        let dir = tempfile::tempdir().unwrap();
        // Empty raw description (yt-dlp returns empty string).
        tokio::fs::write(dir.path().join("vidEMPTY_description.txt"), "")
            .await
            .unwrap();

        // AiClient at unreachable URL — Claude must NOT be called for empty descriptions.
        let ai = AiClient::new(AiSettings {
            api_url: "http://127.0.0.1:1/v1".into(),
            api_key: Some("never".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });
        let bogus_ytdlp = Path::new("/definitely/does/not/exist/ytdlp");

        let out = fetch_description_lyrics(&ai, bogus_ytdlp, "vidEMPTY", dir.path(), "S", "A")
            .await
            .unwrap();
        assert_eq!(out, None);

        let cache = read_lyrics_cache(&dir.path().join("vidEMPTY_description_lyrics.json"))
            .await
            .unwrap();
        assert_eq!(cache, Some(None), "empty description must cache null");
    }

    #[tokio::test]
    async fn clean_lyrics_via_claude_returns_parsed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidCLEAN_genius_cleaned.json");

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "{\"lines\": [\"Line A\", \"Line B\"]}"
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

        let out = clean_lyrics_via_claude(
            &ai,
            "Song",
            "Artist",
            "Line A\nLine B\nLine A\nLine B",
            &cache_path,
            CleanupMode::ScrapedLyrics,
        )
        .await
        .unwrap();

        assert_eq!(out, Some(vec!["Line A".to_string(), "Line B".to_string()]));
        // Cache file written.
        let cache = read_lyrics_cache(&cache_path).await.unwrap();
        assert_eq!(cache, Some(Some(vec!["Line A".into(), "Line B".into()])));
    }

    #[tokio::test]
    async fn clean_lyrics_via_claude_uses_cache_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidCACHED_genius_cleaned.json");

        // Pre-seed the cache to skip the Claude call entirely.
        write_lyrics_cache(
            &cache_path,
            Some(&["cached A".to_string(), "cached B".to_string()]),
        )
        .await
        .unwrap();

        // Intentionally bogus AI URL — if the helper ignores cache and tries
        // to call Claude, the call will fail (and the test will fail with Err
        // instead of the expected cached value).
        let ai = AiClient::new(AiSettings {
            api_url: "http://127.0.0.1:1/v1".into(),
            api_key: Some("test".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });

        let out = clean_lyrics_via_claude(
            &ai,
            "Song",
            "Artist",
            "raw input",
            &cache_path,
            CleanupMode::ScrapedLyrics,
        )
        .await
        .unwrap();

        assert_eq!(out, Some(vec!["cached A".into(), "cached B".into()]));
    }

    #[tokio::test]
    async fn clean_lyrics_via_claude_returns_err_on_claude_error() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidERR_genius_cleaned.json");

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let ai = AiClient::new(AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });

        let out = clean_lyrics_via_claude(
            &ai,
            "Song",
            "Artist",
            "raw input",
            &cache_path,
            CleanupMode::ScrapedLyrics,
        )
        .await;

        assert!(out.is_err(), "expected Err on Claude 500, got: {out:?}");
        // Cache file must NOT exist — failed runs are retryable.
        assert!(!cache_path.exists(), "cache file should not exist on Err");
    }

    #[tokio::test]
    async fn clean_lyrics_via_claude_returns_none_when_claude_says_null() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidNULL_genius_cleaned.json");

        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "{\"lines\": null}"
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

        let out = clean_lyrics_via_claude(
            &ai,
            "Song",
            "Artist",
            "buy my album",
            &cache_path,
            CleanupMode::ScrapedLyrics,
        )
        .await
        .unwrap();

        assert_eq!(out, None);
        // Null decision IS cached so we don't re-call Claude.
        let cache = read_lyrics_cache(&cache_path).await.unwrap();
        assert_eq!(cache, Some(None));
    }

    /// Sanity-checks the dedup / ad-lib / hype-intro instructions are present
    /// in the scraped-lyrics prompt. Catches accidental prompt regressions that
    /// would silently fall back to description-style behavior (which on genius
    /// input produces no cleanup — confirmed in production on id=233 Saints,
    /// 2026-05-11).
    #[test]
    fn scraped_lyrics_prompt_mentions_dedup_adlib_intro_rules() {
        let (system, user) = build_scraped_lyrics_cleanup_prompt(
            "Saints",
            "planetboom",
            "Has He changed your life?\nIt's the power of Jesus\nIt's the power of Jesus",
        );
        assert!(
            system.is_empty(),
            "soft-framing must use empty system prompt"
        );
        // Dedup must be explicit; mere "duplicate" is not enough — test for the
        // distinguishing word "consecutive" so paraphrases that lose the rule fail.
        assert!(
            user.to_lowercase().contains("dedupe") || user.to_lowercase().contains("dedup"),
            "scraped-lyrics prompt must instruct dedup of consecutive identical lines"
        );
        assert!(
            user.to_lowercase().contains("consecutive"),
            "scraped-lyrics prompt must mention 'consecutive' (non-consecutive repeats kept)"
        );
        // Ad-libs / vocalizations.
        assert!(
            user.to_lowercase().contains("ad-lib") || user.to_lowercase().contains("vocaliz"),
            "scraped-lyrics prompt must mention ad-libs / vocalizations"
        );
        // Hype intros.
        assert!(
            user.to_lowercase().contains("intro"),
            "scraped-lyrics prompt must mention DJ / hype intros"
        );
        // Title / artist / blob must appear.
        assert!(user.contains("Saints"), "title missing from prompt");
        assert!(user.contains("planetboom"), "artist missing from prompt");
        assert!(
            user.contains("It's the power of Jesus"),
            "raw blob missing from prompt"
        );
        // JSON output contract preserved.
        assert!(
            user.contains("\"lines\""),
            "scraped-lyrics prompt must request `lines` JSON key"
        );
    }

    /// Verifies `clean_lyrics_via_claude` actually dispatches on `CleanupMode`.
    /// A regression in the match arm (e.g., both modes using the description
    /// prompt) would silently make the genius path useless. The test stubs
    /// Claude and inspects the request body to confirm the SCRAPED prompt's
    /// rule-text was sent, not the description prompt's.
    #[tokio::test]
    async fn clean_lyrics_via_claude_dispatches_scraped_prompt_on_scrapedmode() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use wiremock::matchers::{method, path};

        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("vidMODE_genius_cleaned_v2.json");
        let captured_body = Arc::new(Mutex::new(String::new()));

        let mock = wiremock::MockServer::start().await;
        let captured = captured_body.clone();
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &wiremock::Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                *captured.lock().unwrap() = body;
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "{\"lines\": [\"x\"]}"
                        }
                    }]
                }))
            })
            .mount(&mock)
            .await;

        let ai = AiClient::new(AiSettings {
            api_url: format!("{}/v1", mock.uri()),
            api_key: Some("test".into()),
            model: "stub".into(),
            system_prompt_extra: None,
        });

        let _ = clean_lyrics_via_claude(
            &ai,
            "Song",
            "Artist",
            "raw",
            &cache_path,
            CleanupMode::ScrapedLyrics,
        )
        .await
        .unwrap();

        let body = captured_body.lock().unwrap().clone();
        // SCRAPED-prompt rule text must appear in the outgoing request, NOT
        // the description-prompt's signature phrases.
        assert!(
            body.to_lowercase().contains("dedup"),
            "ScrapedLyrics mode must send the dedup-rule prompt; body: {body}"
        );
        assert!(
            !body.contains("from this YouTube video description"),
            "ScrapedLyrics mode must NOT send the description-prompt text; body: {body}"
        );
    }
}
