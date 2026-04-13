//! Rust subprocess wrapper for the `lyrics_worker.py` Python ML helper.
//!
//! Provides async functions to drive Qwen3-based alignment and transcription
//! by spawning the Python script as a child process and parsing its JSON output.

use anyhow::{Context, Result};
use serde::Deserialize;
use sp_core::lyrics::{LyricsLine, LyricsWord};
use std::path::Path;
use tokio::fs;
use tokio::process::Command;
use tracing::debug;

// ---------------------------------------------------------------------------
// Python output structures
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AlignOutput {
    lines: Vec<AlignLine>,
}

#[derive(Debug, Deserialize)]
struct AlignLine {
    en: String,
    words: Vec<AlignWord>,
}

#[derive(Debug, Deserialize)]
struct AlignWord {
    text: String,
    start_ms: u64,
    end_ms: u64,
}

#[derive(Debug, Deserialize)]
struct TranscribeOutput {
    text: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Align `lyrics_text` to `audio_path` using Qwen3-ForcedAligner-0.6B.
///
/// Writes a temporary `.txt` file for the lyrics, invokes the Python helper,
/// reads the resulting JSON, and returns the parsed `Vec<LyricsLine>`.
/// The temp file and output file are cleaned up after parsing.
pub async fn align_lyrics(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_path: &Path,
    lyrics_text: &str,
    output_path: &Path,
) -> Result<Vec<LyricsLine>> {
    // Write lyrics to a temp file
    let temp_txt = output_path.with_extension("lyrics_tmp.txt");
    fs::write(&temp_txt, lyrics_text)
        .await
        .context("failed to write temporary lyrics text file")?;

    // Build the command
    let mut cmd = Command::new(python_path);
    cmd.args([
        script_path.as_os_str(),
        "align".as_ref(),
        "--audio".as_ref(),
        audio_path.as_os_str(),
        "--text".as_ref(),
        temp_txt.as_os_str(),
        "--output".as_ref(),
        output_path.as_os_str(),
        "--models-dir".as_ref(),
        models_dir.as_os_str(),
    ]);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    debug!(
        "Running aligner: {} align --audio {} --text {} --output {} --models-dir {}",
        python_path.display(),
        audio_path.display(),
        temp_txt.display(),
        output_path.display(),
        models_dir.display(),
    );

    let status = cmd
        .status()
        .await
        .context("failed to spawn lyrics_worker.py align")?;

    // Clean up temp lyrics file regardless of outcome
    let _ = fs::remove_file(&temp_txt).await;

    if !status.success() {
        anyhow::bail!("lyrics_worker.py align exited with status {}", status);
    }

    // Read and parse JSON output
    let json_bytes = fs::read(output_path)
        .await
        .context("failed to read aligner output JSON")?;

    let _ = fs::remove_file(output_path).await;

    let parsed: AlignOutput =
        serde_json::from_slice(&json_bytes).context("failed to parse aligner JSON output")?;

    Ok(convert_align_output(parsed))
}

/// Transcribe `audio_path` using Qwen3-ASR-1.7B.
///
/// Returns the transcribed text string.
pub async fn transcribe_audio(
    python_path: &Path,
    script_path: &Path,
    models_dir: &Path,
    audio_path: &Path,
    output_path: &Path,
) -> Result<String> {
    let mut cmd = Command::new(python_path);
    cmd.args([
        script_path.as_os_str(),
        "transcribe".as_ref(),
        "--audio".as_ref(),
        audio_path.as_os_str(),
        "--output".as_ref(),
        output_path.as_os_str(),
        "--models-dir".as_ref(),
        models_dir.as_os_str(),
    ]);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    debug!(
        "Running transcriber: {} transcribe --audio {} --output {} --models-dir {}",
        python_path.display(),
        audio_path.display(),
        output_path.display(),
        models_dir.display(),
    );

    let status = cmd
        .status()
        .await
        .context("failed to spawn lyrics_worker.py transcribe")?;

    if !status.success() {
        anyhow::bail!("lyrics_worker.py transcribe exited with status {}", status);
    }

    let json_bytes = fs::read(output_path)
        .await
        .context("failed to read transcribe output JSON")?;

    let _ = fs::remove_file(output_path).await;

    let parsed: TranscribeOutput =
        serde_json::from_slice(&json_bytes).context("failed to parse transcribe JSON output")?;

    Ok(parsed.text)
}

/// Check whether the Python environment has a CUDA-capable GPU available.
///
/// Runs `lyrics_worker.py check-gpu` and parses the `"gpu"` field.
pub async fn check_gpu(python_path: &Path, script_path: &Path) -> Result<bool> {
    let mut cmd = Command::new(python_path);
    cmd.args([script_path.as_os_str(), "check-gpu".as_ref()]);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let output = cmd
        .output()
        .await
        .context("failed to spawn lyrics_worker.py check-gpu")?;

    if !output.status.success() {
        anyhow::bail!(
            "lyrics_worker.py check-gpu exited with status {}",
            output.status
        );
    }

    #[derive(Deserialize)]
    struct GpuOutput {
        gpu: bool,
    }

    let parsed: GpuOutput =
        serde_json::from_slice(&output.stdout).context("failed to parse check-gpu JSON output")?;

    Ok(parsed.gpu)
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn convert_align_output(output: AlignOutput) -> Vec<LyricsLine> {
    output
        .lines
        .into_iter()
        .map(|line| {
            let words: Vec<LyricsWord> = line
                .words
                .into_iter()
                .map(|w| LyricsWord {
                    text: w.text,
                    start_ms: w.start_ms,
                    end_ms: w.end_ms,
                })
                .collect();

            // Derive line timing from first/last word, or default to 0
            let start_ms = words.first().map(|w| w.start_ms).unwrap_or(0);
            let end_ms = words.last().map(|w| w.end_ms).unwrap_or(0);

            LyricsLine {
                start_ms,
                end_ms,
                en: line.en,
                sk: None,
                words: if words.is_empty() { None } else { Some(words) },
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_align_output() {
        let json = r#"{
            "lines": [
                {
                    "en": "Hello world",
                    "words": [
                        {"text": "Hello", "start_ms": 100, "end_ms": 500},
                        {"text": "world", "start_ms": 600, "end_ms": 1000}
                    ]
                },
                {
                    "en": "Foo bar baz",
                    "words": [
                        {"text": "Foo", "start_ms": 1100, "end_ms": 1300},
                        {"text": "bar", "start_ms": 1400, "end_ms": 1600},
                        {"text": "baz", "start_ms": 1700, "end_ms": 2000}
                    ]
                }
            ]
        }"#;

        let parsed: AlignOutput = serde_json::from_str(json).expect("parse AlignOutput");
        assert_eq!(parsed.lines.len(), 2);

        let first = &parsed.lines[0];
        assert_eq!(first.en, "Hello world");
        assert_eq!(first.words.len(), 2);
        assert_eq!(first.words[0].text, "Hello");
        assert_eq!(first.words[0].start_ms, 100);
        assert_eq!(first.words[0].end_ms, 500);
        assert_eq!(first.words[1].text, "world");
        assert_eq!(first.words[1].start_ms, 600);
        assert_eq!(first.words[1].end_ms, 1000);

        let second = &parsed.lines[1];
        assert_eq!(second.en, "Foo bar baz");
        assert_eq!(second.words.len(), 3);
        assert_eq!(second.words[2].text, "baz");
        assert_eq!(second.words[2].end_ms, 2000);
    }

    #[test]
    fn test_parse_align_output_empty_lines() {
        let json = r#"{"lines": []}"#;
        let parsed: AlignOutput = serde_json::from_str(json).expect("parse empty AlignOutput");
        assert_eq!(parsed.lines.len(), 0);
    }

    #[test]
    fn test_parse_align_output_empty_words() {
        let json = r#"{
            "lines": [
                {"en": "Instrumental", "words": []}
            ]
        }"#;
        let parsed: AlignOutput =
            serde_json::from_str(json).expect("parse AlignOutput empty words");
        assert_eq!(parsed.lines[0].en, "Instrumental");
        assert_eq!(parsed.lines[0].words.len(), 0);
    }

    #[test]
    fn test_parse_transcribe_output() {
        let json = r#"{"text": "Hello this is a transcription"}"#;
        let parsed: TranscribeOutput = serde_json::from_str(json).expect("parse TranscribeOutput");
        assert_eq!(parsed.text, "Hello this is a transcription");
    }

    #[test]
    fn test_parse_transcribe_output_empty() {
        let json = r#"{"text": ""}"#;
        let parsed: TranscribeOutput =
            serde_json::from_str(json).expect("parse TranscribeOutput empty");
        assert_eq!(parsed.text, "");
    }

    #[test]
    fn test_convert_align_output_to_lyrics_lines() {
        let output = AlignOutput {
            lines: vec![
                AlignLine {
                    en: "Amazing grace".to_string(),
                    words: vec![
                        AlignWord {
                            text: "Amazing".to_string(),
                            start_ms: 0,
                            end_ms: 400,
                        },
                        AlignWord {
                            text: "grace".to_string(),
                            start_ms: 500,
                            end_ms: 900,
                        },
                    ],
                },
                AlignLine {
                    en: "How sweet the sound".to_string(),
                    words: vec![
                        AlignWord {
                            text: "How".to_string(),
                            start_ms: 1000,
                            end_ms: 1200,
                        },
                        AlignWord {
                            text: "sweet".to_string(),
                            start_ms: 1300,
                            end_ms: 1600,
                        },
                        AlignWord {
                            text: "the".to_string(),
                            start_ms: 1700,
                            end_ms: 1900,
                        },
                        AlignWord {
                            text: "sound".to_string(),
                            start_ms: 2000,
                            end_ms: 2500,
                        },
                    ],
                },
            ],
        };

        let lines = convert_align_output(output);
        assert_eq!(lines.len(), 2);

        let first = &lines[0];
        assert_eq!(first.en, "Amazing grace");
        assert_eq!(first.start_ms, 0);
        assert_eq!(first.end_ms, 900);
        assert!(first.words.is_some());
        let words0 = first.words.as_ref().unwrap();
        assert_eq!(words0.len(), 2);
        assert_eq!(words0[0].text, "Amazing");
        assert_eq!(words0[1].text, "grace");

        let second = &lines[1];
        assert_eq!(second.en, "How sweet the sound");
        assert_eq!(second.start_ms, 1000);
        assert_eq!(second.end_ms, 2500);
        assert_eq!(second.words.as_ref().unwrap().len(), 4);
    }

    #[test]
    fn test_convert_align_output_empty_words_gives_none() {
        let output = AlignOutput {
            lines: vec![AlignLine {
                en: "Silence".to_string(),
                words: vec![],
            }],
        };
        let lines = convert_align_output(output);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].words.is_none());
        assert_eq!(lines[0].start_ms, 0);
        assert_eq!(lines[0].end_ms, 0);
    }
}
