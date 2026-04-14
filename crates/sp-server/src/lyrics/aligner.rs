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
#[cfg_attr(test, mutants::skip)]
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

    let mut child = cmd
        .spawn()
        .context("failed to spawn lyrics_worker.py align")?;

    let timeout = std::time::Duration::from_secs(120);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = fs::remove_file(&temp_txt).await;
            anyhow::bail!("lyrics_worker.py align failed: {e}");
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = fs::remove_file(&temp_txt).await;
            anyhow::bail!("lyrics_worker.py align timed out after {timeout:?}");
        }
    };

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
#[cfg_attr(test, mutants::skip)]
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

    let mut child = cmd
        .spawn()
        .context("failed to spawn lyrics_worker.py transcribe")?;

    let timeout = std::time::Duration::from_secs(300);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => anyhow::bail!("lyrics_worker.py transcribe failed: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("lyrics_worker.py transcribe timed out after {timeout:?}");
        }
    };

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
#[cfg_attr(test, mutants::skip)]
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

/// Merge aligned-word timings into LRCLIB-sourced lines.
///
/// Preserves each LRCLIB line's `start_ms` / `end_ms` / `en` text and
/// attaches the aligned `words` from the matching aligned line by index.
/// Aligned lines beyond `lrclib.len()` are dropped. LRCLIB lines beyond
/// `aligned.len()` keep `words = None`.
pub fn merge_word_timings(lrclib: Vec<LyricsLine>, aligned: Vec<LyricsLine>) -> Vec<LyricsLine> {
    let mut aligned_iter = aligned.into_iter();
    lrclib
        .into_iter()
        .map(|mut line| {
            if let Some(a) = aligned_iter.next() {
                line.words = a.words;
            }
            line
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

    fn lrclib_line(start_ms: u64, end_ms: u64, en: &str) -> LyricsLine {
        LyricsLine {
            start_ms,
            end_ms,
            en: en.to_string(),
            sk: None,
            words: None,
        }
    }

    fn aligned_line(en: &str, words: Vec<(u64, u64, &str)>) -> LyricsLine {
        LyricsLine {
            start_ms: words.first().map(|w| w.0).unwrap_or(0),
            end_ms: words.last().map(|w| w.1).unwrap_or(0),
            en: en.to_string(),
            sk: None,
            words: Some(
                words
                    .into_iter()
                    .map(|(s, e, t)| LyricsWord {
                        text: t.to_string(),
                        start_ms: s,
                        end_ms: e,
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn merge_word_timings_same_count_preserves_lrclib_timing() {
        let lrclib = vec![
            lrclib_line(1000, 3000, "Hello world"),
            lrclib_line(3500, 5000, "Amazing grace"),
        ];
        let aligned = vec![
            aligned_line(
                "Hello world",
                vec![(1100, 1500, "Hello"), (1600, 2200, "world")],
            ),
            aligned_line(
                "Amazing grace",
                vec![(3600, 4200, "Amazing"), (4300, 4900, "grace")],
            ),
        ];
        let out = merge_word_timings(lrclib, aligned);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_ms, 1000, "lrclib start_ms preserved");
        assert_eq!(out[0].end_ms, 3000, "lrclib end_ms preserved");
        assert_eq!(out[0].en, "Hello world");
        let words0 = out[0].words.as_ref().expect("words present");
        assert_eq!(words0.len(), 2);
        assert_eq!(words0[0].text, "Hello");
        assert_eq!(words0[1].text, "world");
        assert_eq!(out[1].start_ms, 3500);
    }

    #[test]
    fn merge_word_timings_fewer_aligned_leaves_tail_unaligned() {
        let lrclib = vec![
            lrclib_line(0, 1000, "Line one"),
            lrclib_line(1000, 2000, "Line two"),
            lrclib_line(2000, 3000, "Line three"),
        ];
        let aligned = vec![aligned_line(
            "Line one",
            vec![(0, 500, "Line"), (500, 1000, "one")],
        )];
        let out = merge_word_timings(lrclib, aligned);
        assert_eq!(out.len(), 3);
        assert!(out[0].words.is_some());
        assert!(out[1].words.is_none(), "unaligned line stays wordless");
        assert!(out[2].words.is_none());
    }

    #[test]
    fn merge_word_timings_more_aligned_ignores_extras() {
        let lrclib = vec![lrclib_line(0, 1000, "Only one line")];
        let aligned = vec![
            aligned_line("Only one line", vec![(0, 500, "Only")]),
            aligned_line("Phantom extra", vec![(500, 1000, "Phantom")]),
        ];
        let out = merge_word_timings(lrclib, aligned);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].en, "Only one line");
        assert_eq!(out[0].words.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn merge_word_timings_empty_aligned_returns_lrclib_unchanged() {
        let lrclib = vec![lrclib_line(0, 1000, "Line one")];
        let out = merge_word_timings(lrclib.clone(), vec![]);
        assert_eq!(out, lrclib);
    }
}
