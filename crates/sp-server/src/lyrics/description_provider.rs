//! YouTube description lyrics provider.
//!
//! Fetches the raw description via yt-dlp, pipes it through a narrow Claude
//! prompt, and emits a `CandidateText { source: "description" }` for the
//! ensemble text-merge step. Caches both the raw description and the
//! extracted lyrics JSON on disk so reprocesses reuse the work.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::ai::client::AiClient;

/// Build the Claude extraction prompt for a single video description.
///
/// The system prompt is intentionally specific: request a JSON object with
/// one `lines` key (array of strings or null), strip section markers, keep
/// non-English as-is, and refuse to fabricate. Returns `(system, user)`.
pub fn build_description_extraction_prompt(
    title: &str,
    artist: &str,
    description: &str,
) -> (String, String) {
    let system = String::from(
        "You are a lyrics extractor. Given a YouTube video description, return the song's lyrics \
         as a JSON object with exactly one key, \"lines\", whose value is either:\n\
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
            return {\"lines\": null}.\n\
         6. Output ONLY the JSON object. No preamble, no markdown fences, no commentary.",
    );
    let user =
        format!("Video title: {title}\nArtist: {artist}\n\nDescription:\n---\n{description}\n---");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_has_rule_about_null_when_no_lyrics() {
        let (system, _user) =
            build_description_extraction_prompt("Song", "Artist", "some description");
        assert!(
            system.contains("null"),
            "system prompt must mention the null case: {system}"
        );
        assert!(
            system.contains("\"lines\""),
            "system prompt must name the JSON key: {system}"
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
        let (system, _user) = build_description_extraction_prompt("S", "A", "desc");
        assert!(
            system.contains("fabricate") || system.contains("not confident"),
            "system prompt must warn against fabrication: {system}"
        );
    }

    #[test]
    fn prompt_requires_original_language() {
        let (system, _user) = build_description_extraction_prompt("S", "A", "desc");
        assert!(
            system.contains("Preserve") && system.contains("translate"),
            "system prompt must require original-language preservation: {system}"
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
}
