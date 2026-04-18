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

/// Build the Claude extraction prompt for a single video description.
///
/// Empty system prompt — soft-framing in user message instead. Mirrors the
/// `text_merge.rs` pattern: CLIProxyAPI OAuth Claude reverts to conversational
/// mode on lyrics content when given a direct-instruction system prompt,
/// producing preamble instead of JSON. Framing the task as "I'm building a
/// karaoke app" positions Claude as a software engineer and makes JSON output
/// reliable. Returns `(system, user)`.
pub fn build_description_extraction_prompt(
    title: &str,
    artist: &str,
    description: &str,
) -> (String, String) {
    // Empty system prompt: soft-framing in user message instead. Matches the
    // text_merge.rs pattern — CLIProxyAPI OAuth Claude refuses to produce
    // structured JSON from a direct "extract lyrics" system prompt because
    // song lyrics trigger content-policy caution and Claude reverts to
    // conversational mode. Framing the task as "I'm building a karaoke app"
    // positions Claude as a software engineer and makes JSON output reliable.
    let system = String::new();
    let user = format!(
        "I'm building a karaoke subtitle app for a church. I need to extract the song's \
         lyrics from this YouTube video description so my app can display them synced to \
         the music.\n\n\
         Return a JSON object with exactly one key, \"lines\", whose value is either:\n\
           - an array of strings (one per lyric line, in reading order, in the song's original language), OR\n\
           - null, when the description contains NO lyrics.\n\
         \n\
         Rules:\n\
         1. Strip section markers (\"Verse 1:\", \"Chorus:\", \"Bridge:\", etc.), keep the line text.\n\
         2. Preserve non-English lyrics as-is. Do NOT translate.\n\
         3. Ignore: artist bio, social links, streaming/buy links, copyright notices, producer/\n\
            writer credits, album promo, tour dates, comment/like/subscribe prompts.\n\
         4. If multiple languages appear (e.g., English + Spanish side-by-side or verse/translation \
            blocks), include ALL lines in reading order — downstream reconciliation handles dedupe.\n\
         5. Do not fabricate lyrics. If you are not confident the text is the song's lyrics, \
            return {{\"lines\": null}}.\n\
         6. Output ONLY the JSON object. No preamble, no markdown fences, no commentary. \
            Start your response with {{ and end with }}.\n\n\
         Video title: {title}\n\
         Artist: {artist}\n\n\
         Description:\n\
         ---\n\
         {description}\n\
         ---"
    );
    (system, user)
}

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

    // Fast path: cached lyrics decision already on disk.
    if let Some(cached) = read_lyrics_cache(&lyrics_cache_path).await? {
        debug!(
            youtube_id,
            "description_provider: cache hit (extracted lyrics)"
        );
        return Ok(cached);
    }

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

    // Call Claude. On any error, return Ok(None) WITHOUT writing a cache
    // entry so the next reprocess retries.
    let (system, user) = build_description_extraction_prompt(title, artist, &description);
    let raw = match ai.chat_with_timeout(&system, &user, 180).await {
        Ok(r) => r,
        Err(e) => {
            warn!(youtube_id, %e, "description_provider: Claude extraction failed");
            return Ok(None);
        }
    };
    let parsed = match parse_claude_response(&raw) {
        Ok(p) => p,
        Err(e) => {
            warn!(youtube_id, %e, "description_provider: Claude response malformed");
            return Ok(None);
        }
    };
    // Persist the decision (Some or None) so next reprocess skips Claude.
    write_lyrics_cache(&lyrics_cache_path, parsed.as_deref()).await?;
    Ok(parsed)
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
}
