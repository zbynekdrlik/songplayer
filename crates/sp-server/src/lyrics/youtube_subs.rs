//! YouTube subtitle fetcher and json3 format parser.

use anyhow::Result;
use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsTrack};
use std::path::Path;
use tokio::process::Command;
use tracing::debug;

// ---------------------------------------------------------------------------
// json3 format structures
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Json3Root {
    events: Option<Vec<Json3Event>>,
}

#[derive(Debug, Deserialize)]
struct Json3Event {
    #[serde(default, rename = "tStartMs")]
    t_start_ms: u64,
    #[serde(default, rename = "dDurationMs")]
    d_duration_ms: u64,
    #[serde(default)]
    segs: Option<Vec<Json3Seg>>,
}

#[derive(Debug, Deserialize)]
struct Json3Seg {
    #[serde(default)]
    utf8: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Download subtitles for `youtube_id` via yt-dlp into `temp_dir`,
/// parse the resulting json3 file, clean up, and return a `LyricsTrack`.
///
/// Returns `None` if yt-dlp fails or no subtitle file is found.
#[cfg_attr(test, mutants::skip)]
pub async fn fetch_subtitles(
    ytdlp_path: &Path,
    youtube_id: &str,
    temp_dir: &Path,
) -> Result<Option<LyricsTrack>> {
    let output_template = temp_dir.join(youtube_id).to_string_lossy().into_owned();
    let url = format!("https://www.youtube.com/watch?v={}", youtube_id);

    let mut cmd = Command::new(ytdlp_path);
    cmd.args([
        "--write-subs",
        "--write-auto-subs",
        "--sub-format",
        "json3",
        "--sub-lang",
        "en",
        "--skip-download",
        "-o",
        &output_template,
        &url,
    ]);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let status = cmd.status().await;

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            debug!("yt-dlp exited with status {} for video {}", s, youtube_id);
            return Ok(None);
        }
        Err(e) => {
            debug!("yt-dlp failed to run for video {}: {}", youtube_id, e);
            return Ok(None);
        }
    }

    // Find the .json3 file yt-dlp wrote
    let json3_path = find_json3_file(temp_dir, youtube_id)?;

    let Some(json3_path) = json3_path else {
        debug!("No .json3 subtitle file found for video {}", youtube_id);
        return Ok(None);
    };

    let content = tokio::fs::read_to_string(&json3_path).await?;

    // Clean up temp file regardless of parse result
    let _ = tokio::fs::remove_file(&json3_path).await;

    parse_json3(&content)
}

/// Scan `dir` for a file matching `*{youtube_id}*.json3`.
fn find_json3_file(dir: &Path, youtube_id: &str) -> Result<Option<std::path::PathBuf>> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(None),
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.contains(youtube_id) && name.ends_with(".json3") {
                return Ok(Some(path));
            }
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// json3 parser
// ---------------------------------------------------------------------------

/// Parse YouTube json3 subtitle content into a `LyricsTrack`.
///
/// Each event becomes one line. Segments within an event are concatenated.
/// Newlines in text are replaced with spaces. Empty lines are skipped.
/// Returns `None` if there are no events or all lines are empty.
pub fn parse_json3(content: &str) -> Result<Option<LyricsTrack>> {
    let root: Json3Root = serde_json::from_str(content)?;

    let events = match root.events {
        Some(e) if !e.is_empty() => e,
        _ => return Ok(None),
    };

    let mut lines: Vec<LyricsLine> = Vec::new();

    for event in events {
        let segs = match event.segs {
            Some(s) => s,
            None => continue,
        };

        // Concatenate all segment text
        let text: String = segs.iter().map(|s| s.utf8.as_str()).collect();
        // Replace newlines with spaces and trim
        let text = text.replace('\n', " ");
        let text = text.trim().to_string();

        if text.is_empty() {
            continue;
        }

        let start_ms = event.t_start_ms;
        let end_ms = start_ms + event.d_duration_ms;

        lines.push(LyricsLine {
            start_ms,
            end_ms,
            en: text,
            sk: None,
            words: None,
        });
    }

    if lines.is_empty() {
        return Ok(None);
    }

    Ok(Some(LyricsTrack {
        version: 1,
        source: "youtube".to_string(),
        language_source: "en".to_string(),
        language_translation: String::new(),
        lines,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json3_basic_two_events() {
        let content = r#"{
            "events": [
                {
                    "tStartMs": 1000,
                    "dDurationMs": 2000,
                    "segs": [{"utf8": "Hello world"}]
                },
                {
                    "tStartMs": 5000,
                    "dDurationMs": 3000,
                    "segs": [{"utf8": "Goodbye"}]
                }
            ]
        }"#;

        let track = parse_json3(content).unwrap().expect("should parse");
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.source, "youtube");

        assert_eq!(track.lines[0].start_ms, 1000);
        assert_eq!(track.lines[0].end_ms, 3000);
        assert_eq!(track.lines[0].en, "Hello world");

        assert_eq!(track.lines[1].start_ms, 5000);
        assert_eq!(track.lines[1].end_ms, 8000);
        assert_eq!(track.lines[1].en, "Goodbye");
    }

    #[test]
    fn parse_json3_skips_empty_segments() {
        let content = r#"{
            "events": [
                {
                    "tStartMs": 1000,
                    "dDurationMs": 2000,
                    "segs": [{"utf8": "  "}, {"utf8": "\n"}]
                },
                {
                    "tStartMs": 5000,
                    "dDurationMs": 3000,
                    "segs": [{"utf8": "Real line"}]
                }
            ]
        }"#;

        let track = parse_json3(content).unwrap().expect("should parse");
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].en, "Real line");
    }

    #[test]
    fn parse_json3_empty_events_returns_none() {
        let content = r#"{"events": []}"#;
        let result = parse_json3(content).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_json3_no_events_field_returns_none() {
        let content = r#"{}"#;
        let result = parse_json3(content).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_json3_replaces_newlines_in_text() {
        let content = r#"{
            "events": [
                {
                    "tStartMs": 0,
                    "dDurationMs": 1000,
                    "segs": [{"utf8": "Line one\nLine two"}]
                }
            ]
        }"#;

        let track = parse_json3(content).unwrap().expect("should parse");
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].en, "Line one Line two");
    }

    #[test]
    fn parse_json3_multiple_segs_concatenated() {
        let content = r#"{
            "events": [
                {
                    "tStartMs": 2000,
                    "dDurationMs": 4000,
                    "segs": [{"utf8": "Hello"}, {"utf8": " "}, {"utf8": "world"}]
                }
            ]
        }"#;

        let track = parse_json3(content).unwrap().expect("should parse");
        assert_eq!(track.lines[0].en, "Hello world");
        assert_eq!(track.lines[0].start_ms, 2000);
        assert_eq!(track.lines[0].end_ms, 6000);
    }

    #[test]
    fn parse_json3_skips_events_with_no_segs() {
        let content = r#"{
            "events": [
                {
                    "tStartMs": 1000,
                    "dDurationMs": 2000
                },
                {
                    "tStartMs": 5000,
                    "dDurationMs": 3000,
                    "segs": [{"utf8": "Valid"}]
                }
            ]
        }"#;

        let track = parse_json3(content).unwrap().expect("should parse");
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].en, "Valid");
    }

    #[test]
    fn parse_json3_all_empty_lines_returns_none() {
        let content = r#"{
            "events": [
                {
                    "tStartMs": 1000,
                    "dDurationMs": 2000,
                    "segs": [{"utf8": "   "}]
                }
            ]
        }"#;

        let result = parse_json3(content).unwrap();
        assert!(result.is_none());
    }
}
