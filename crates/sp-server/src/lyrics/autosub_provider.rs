//! YouTube auto-sub transfer alignment provider.
//!
//! Pulls word timestamps from yt-dlp's json3 caption format and matches them
//! against the orchestrator's selected reference text using the sequential
//! forward-walk matcher ported from `scripts/experiments/autosub_drift.py`.
//!
//! Density gate neutralizes worship-fast songs where YouTube ASR collapses:
//! densities below 0.3 words/sec fail `can_provide`, so the merge layer only
//! receives autosub results when they're likely to contribute signal.

use crate::lyrics::provider::{
    AlignmentProvider, CandidateText, LineTiming, ProviderResult, SongContext, WordTiming,
};
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Known YouTube auto-sub noise tokens that should never participate in word matching.
/// Kept at module scope so `normalize_word`, `parse_json3`, and the matcher all use
/// the same source of truth.
const NOISE_TOKENS: &[&str] = &["[music]", ">>", "[applause]", "[laughter]"];

/// A single word from the json3 auto-sub stream.
#[derive(Debug, Clone, PartialEq)]
pub struct AutosubWord {
    pub text: String,
    pub start_ms: u64,
}

/// Normalize a word for matching: lowercase, strip `[^\w]`, drop noise tokens.
/// Returns empty string for noise/empty/whitespace input.
pub fn normalize_word(s: &str) -> String {
    let trimmed = s.trim().to_lowercase();
    if trimmed.is_empty() || NOISE_TOKENS.iter().any(|n| trimmed == *n) {
        return String::new();
    }
    trimmed
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Parse yt-dlp's json3 caption format into a flat word stream. Handles both
/// word-level captions (each seg has tOffsetMs) and sentence-level captions
/// (one seg per event — split on whitespace, assign event start_ms to every word).
pub fn parse_json3(json_text: &str) -> anyhow::Result<Vec<AutosubWord>> {
    let doc: serde_json::Value = serde_json::from_str(json_text)?;
    let events = doc.get("events").and_then(|v| v.as_array());
    let Some(events) = events else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for event in events {
        let segs = event.get("segs").and_then(|v| v.as_array());
        let Some(segs) = segs else { continue };
        if segs.is_empty() {
            continue;
        }
        let event_start = event.get("tStartMs").and_then(|v| v.as_i64()).unwrap_or(0) as u64;

        let word_level = segs.iter().any(|s| s.get("tOffsetMs").is_some());
        if word_level {
            for seg in segs {
                let fragment = seg
                    .get("utf8")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if fragment.is_empty() {
                    continue;
                }
                let offset = seg.get("tOffsetMs").and_then(|v| v.as_i64()).unwrap_or(0) as u64;
                out.push(AutosubWord {
                    text: fragment.to_string(),
                    start_ms: event_start + offset,
                });
            }
        } else {
            let joined: String = segs
                .iter()
                .filter_map(|s| s.get("utf8").and_then(|v| v.as_str()))
                .collect();
            for word in joined.split_whitespace() {
                out.push(AutosubWord {
                    text: word.to_string(),
                    start_ms: event_start,
                });
            }
        }
    }

    // Quietly drop known noise tokens at parse time so downstream matcher doesn't see them.
    out.retain(|w| {
        !NOISE_TOKENS
            .iter()
            .any(|n| *n == w.text.to_lowercase().as_str())
    });
    Ok(out)
}

/// Per-reference-word match result from the forward walker.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchedWord {
    pub reference_text: String,
    pub autosub_start_ms: Option<u64>,
}

/// Sequential forward walker, ported from scripts/experiments/autosub_drift.py.
///
/// For each reference word, search up to `window` autosub words ahead for the
/// first exact-text match after normalization. On match: record start_ms and
/// advance autosub pointer. On miss: return None for that word; autosub pointer
/// stays where it was. No backtracking — drift recovers on the next match.
pub fn match_reference_to_autosub(
    reference_words: &[&str],
    autosub_words: &[AutosubWord],
    window: usize,
) -> Vec<MatchedWord> {
    let mut out = Vec::with_capacity(reference_words.len());
    let mut auto_idx = 0usize;

    for r in reference_words {
        let r_norm = normalize_word(r);
        if r_norm.is_empty() {
            out.push(MatchedWord {
                reference_text: (*r).to_string(),
                autosub_start_ms: None,
            });
            continue;
        }

        let mut found = None;
        for offset in 0..window {
            let cand_idx = auto_idx + offset;
            if cand_idx >= autosub_words.len() {
                break;
            }
            if normalize_word(&autosub_words[cand_idx].text) == r_norm {
                found = Some(cand_idx);
                break;
            }
        }

        match found {
            Some(idx) => {
                out.push(MatchedWord {
                    reference_text: (*r).to_string(),
                    autosub_start_ms: Some(autosub_words[idx].start_ms),
                });
                auto_idx = idx + 1;
            }
            None => out.push(MatchedWord {
                reference_text: (*r).to_string(),
                autosub_start_ms: None,
            }),
        }
    }

    out
}

/// Confidence for autosub word timings, gated by density. Worship-fast songs
/// (density < 0.3 wps) get 0.1 so merge layer downweights them. Dense ballads
/// (>= 1.0 wps) get 0.6 matching Qwen3's base confidence.
pub fn density_gate_confidence(words_per_second: f32) -> f32 {
    if words_per_second >= 1.0 {
        0.6
    } else if words_per_second <= 0.3 {
        0.1 // defensive: can_provide already filters wps < 0.3
    } else {
        0.1 + (words_per_second - 0.3) / 0.7 * 0.5
    }
}

pub struct AutoSubProvider;

#[async_trait]
impl AlignmentProvider for AutoSubProvider {
    fn name(&self) -> &str {
        "autosub"
    }

    fn base_confidence(&self) -> f32 {
        0.6
    }

    async fn can_provide(&self, ctx: &SongContext) -> bool {
        let Some(path) = ctx.autosub_json3.as_ref() else {
            return false;
        };
        if !path.exists() {
            return false;
        }
        let raw = match tokio::fs::read_to_string(path).await {
            Ok(s) => s,
            Err(_) => return false,
        };
        let words = match parse_json3(&raw) {
            Ok(w) => w,
            Err(_) => return false,
        };
        if words.len() < 10 || ctx.duration_ms == 0 {
            return false;
        }
        let density = words.len() as f32 / (ctx.duration_ms as f32 / 1000.0);
        density >= 0.3
    }

    async fn align(&self, ctx: &SongContext) -> Result<ProviderResult> {
        let path = ctx
            .autosub_json3
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("autosub path missing"))?;
        let raw = tokio::fs::read_to_string(path).await?;
        let autosub_words = parse_json3(&raw)?;

        let density = autosub_words.len() as f32 / (ctx.duration_ms as f32 / 1000.0);
        let confidence = density_gate_confidence(density);

        // Reference comes from candidate_texts. In Task 7 the orchestrator will
        // Claude-merge candidates into a single "reference"-labeled entry; until
        // then we pick the first candidate. When the merged entry exists, use it.
        let reference = ctx
            .candidate_texts
            .iter()
            .find(|c| c.source == "reference")
            .map(|c| &c.lines)
            .or_else(|| ctx.candidate_texts.first().map(|c| &c.lines))
            .ok_or_else(|| anyhow::anyhow!("no reference text available"))?;

        // Flatten reference text into a word stream; remember each line's word count
        // so we can re-slice matches back into the original line structure.
        let mut flat_ref: Vec<&str> = Vec::new();
        let mut line_word_counts: Vec<usize> = Vec::with_capacity(reference.len());
        for line in reference {
            let count_before = flat_ref.len();
            for w in line.split_whitespace() {
                flat_ref.push(w);
            }
            line_word_counts.push(flat_ref.len() - count_before);
        }

        let matched = match_reference_to_autosub(&flat_ref, &autosub_words, 10);

        // Emit LineTimings. For each line, collect the matched words; use the
        // first/last matched timestamps as line-level start/end. Unmatched reference
        // words are silently skipped — the merge layer fills gaps from other providers.
        let mut lines_out = Vec::with_capacity(reference.len());
        let mut cursor = 0usize;
        for (line_idx, line) in reference.iter().enumerate() {
            let word_count = line_word_counts[line_idx];
            let line_slice = &matched[cursor..cursor + word_count];
            cursor += word_count;

            let words: Vec<WordTiming> = line_slice
                .iter()
                .enumerate()
                .filter_map(|(idx, m)| {
                    let start = m.autosub_start_ms?;
                    // end_ms = next matched start_ms in this line, else start + 500 fallback
                    let end = line_slice
                        .iter()
                        .skip(idx + 1)
                        .find_map(|n| n.autosub_start_ms)
                        .unwrap_or(start + 500);
                    Some(WordTiming {
                        text: m.reference_text.clone(),
                        start_ms: start,
                        end_ms: end,
                        confidence,
                    })
                })
                .collect();

            let (line_start, line_end) = match (words.first(), words.last()) {
                (Some(f), Some(l)) => (f.start_ms, l.end_ms),
                _ => (0, 0),
            };

            lines_out.push(LineTiming {
                text: line.clone(),
                start_ms: line_start,
                end_ms: line_end,
                words,
            });
        }

        Ok(ProviderResult {
            provider_name: "autosub".into(),
            lines: lines_out,
            metadata: serde_json::json!({
                "base_confidence": confidence,
                "density_wps": density,
                "autosub_word_count": autosub_words.len(),
            }),
        })
    }
}

/// Fetch auto-subs for a YouTube video id into `out_dir`. Returns the
/// downloaded json3 path or None if the video has no auto-subs. Errors
/// propagate for real failures (network, banned, malformed args). yt-dlp exits
/// 0 with no file when the video simply has no English auto-subs — that case
/// is reported as Ok(None).
#[cfg_attr(test, mutants::skip)] // I/O-only subprocess wrapper; covered by integration tests (Task 8)
pub async fn fetch_autosub(
    ytdlp_path: &Path,
    video_id: &str,
    out_dir: &Path,
) -> Result<Option<PathBuf>> {
    tokio::fs::create_dir_all(out_dir).await?;
    let out_template = out_dir.join(format!("{video_id}.%(ext)s"));
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let mut cmd = tokio::process::Command::new(ytdlp_path);
    cmd.arg("--write-auto-subs")
        .arg("--sub-format")
        .arg("json3")
        .arg("--sub-langs")
        .arg("en")
        .arg("--skip-download")
        .arg("--no-warnings")
        .arg("-o")
        .arg(&out_template)
        .arg(&url);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.kill_on_drop(true);
    let output = cmd.output().await?;
    if !output.status.success() {
        anyhow::bail!(
            "yt-dlp auto-subs fetch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let primary = out_dir.join(format!("{video_id}.en.json3"));
    if primary.exists() {
        return Ok(Some(primary));
    }
    // When a video has BOTH manual and auto subs, the auto variant gets -orig suffix.
    let orig = out_dir.join(format!("{video_id}.en-orig.json3"));
    if orig.exists() {
        return Ok(Some(orig));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_word_lowercases_and_strips_punct() {
        assert_eq!(normalize_word("Hello!"), "hello");
        assert_eq!(normalize_word("World,"), "world");
        assert_eq!(normalize_word("Don't"), "dont");
        assert_eq!(normalize_word("  PADDED  "), "padded");
    }

    #[test]
    fn normalize_word_drops_noise_tokens() {
        assert_eq!(normalize_word("[music]"), "");
        assert_eq!(normalize_word("[MUSIC]"), "");
        assert_eq!(normalize_word(">>"), "");
        assert_eq!(normalize_word("[applause]"), "");
        assert_eq!(normalize_word("[laughter]"), "");
    }

    #[test]
    fn normalize_word_empty_for_blank_input() {
        assert_eq!(normalize_word(""), "");
        assert_eq!(normalize_word("   "), "");
    }

    #[test]
    fn parse_json3_word_level() {
        let raw = include_str!("../../tests/fixtures/autosub/word_level.json3");
        let words = parse_json3(raw).unwrap();
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["Hello", "world", "how", "are", "you"]);
        // Start times: 1000+0, 1000+250, 2000+0, 2000+300, 2000+600
        assert_eq!(
            words.iter().map(|w| w.start_ms).collect::<Vec<_>>(),
            vec![1000, 1250, 2000, 2300, 2600]
        );
    }

    #[test]
    fn parse_json3_sentence_level_splits_on_whitespace() {
        let raw = include_str!("../../tests/fixtures/autosub/sentence_level.json3");
        let words = parse_json3(raw).unwrap();
        // First event is [music] — dropped as noise.
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["Amazing", "grace", "how", "sweet", "the", "sound"]
        );
        // All words from event 2 share tStartMs = 3000
        for w in &words {
            assert_eq!(w.start_ms, 3000);
        }
    }

    #[test]
    fn parse_json3_empty() {
        let raw = include_str!("../../tests/fixtures/autosub/empty.json3");
        let words = parse_json3(raw).unwrap();
        assert!(words.is_empty());
    }

    #[test]
    fn parse_json3_handles_missing_events_field() {
        let words = parse_json3("{}").unwrap();
        assert!(words.is_empty());
    }

    #[test]
    fn parse_json3_rejects_invalid_json() {
        assert!(parse_json3("not json").is_err());
    }

    #[test]
    fn match_exact_sequential() {
        let ref_words = vec!["Hello", "world", "again"];
        let autosub = vec![
            AutosubWord {
                text: "Hello".into(),
                start_ms: 100,
            },
            AutosubWord {
                text: "world".into(),
                start_ms: 200,
            },
            AutosubWord {
                text: "again".into(),
                start_ms: 300,
            },
        ];
        let out = match_reference_to_autosub(&ref_words, &autosub, 10);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].autosub_start_ms, Some(100));
        assert_eq!(out[1].autosub_start_ms, Some(200));
        assert_eq!(out[2].autosub_start_ms, Some(300));
    }

    #[test]
    fn match_skips_unmatched_reference_words() {
        let ref_words = vec!["Hello", "missing", "world"];
        let autosub = vec![
            AutosubWord {
                text: "Hello".into(),
                start_ms: 100,
            },
            AutosubWord {
                text: "world".into(),
                start_ms: 200,
            },
        ];
        let out = match_reference_to_autosub(&ref_words, &autosub, 10);
        assert_eq!(out[0].autosub_start_ms, Some(100));
        assert_eq!(
            out[1].autosub_start_ms, None,
            "'missing' has no counterpart"
        );
        assert_eq!(out[2].autosub_start_ms, Some(200));
    }

    #[test]
    fn match_window_boundary() {
        let ref_words = vec!["needle"];
        // Autosub has "needle" at index 9 (inside window=10) and 10 (outside window=10)
        let mut autosub: Vec<AutosubWord> = (0..9)
            .map(|i| AutosubWord {
                text: format!("pad{i}"),
                start_ms: i as u64,
            })
            .collect();
        autosub.push(AutosubWord {
            text: "needle".into(),
            start_ms: 999,
        });

        let inside = match_reference_to_autosub(&ref_words, &autosub, 10);
        assert_eq!(inside[0].autosub_start_ms, Some(999));

        let outside = match_reference_to_autosub(&ref_words, &autosub, 9);
        assert_eq!(
            outside[0].autosub_start_ms, None,
            "needle at offset 9 is outside window=9"
        );
    }

    #[test]
    fn match_autosub_pointer_advances_only_on_hit() {
        let ref_words = vec!["a", "missing", "b"];
        let autosub = vec![
            AutosubWord {
                text: "a".into(),
                start_ms: 100,
            },
            AutosubWord {
                text: "b".into(),
                start_ms: 200,
            },
        ];
        let out = match_reference_to_autosub(&ref_words, &autosub, 10);
        assert_eq!(out[0].autosub_start_ms, Some(100));
        assert_eq!(out[1].autosub_start_ms, None);
        assert_eq!(
            out[2].autosub_start_ms,
            Some(200),
            "after miss, pointer stays at 'b' and matches it"
        );
    }

    #[test]
    fn match_normalizes_punctuation() {
        let ref_words = vec!["Hello,", "world!"];
        let autosub = vec![
            AutosubWord {
                text: "hello".into(),
                start_ms: 100,
            },
            AutosubWord {
                text: "World".into(),
                start_ms: 200,
            },
        ];
        let out = match_reference_to_autosub(&ref_words, &autosub, 10);
        assert_eq!(out[0].autosub_start_ms, Some(100));
        assert_eq!(out[1].autosub_start_ms, Some(200));
    }

    #[test]
    fn density_gate_thresholds() {
        assert!((density_gate_confidence(1.0) - 0.6).abs() < 1e-6);
        assert!(
            (density_gate_confidence(1.5) - 0.6).abs() < 1e-6,
            "capped at 0.6"
        );
        assert!((density_gate_confidence(0.3) - 0.1).abs() < 1e-6);
        assert!(
            (density_gate_confidence(0.2) - 0.1).abs() < 1e-6,
            "floored at 0.1"
        );
        // Linear between: at 0.65 wps → 0.1 + (0.35/0.7)*0.5 = 0.35
        assert!((density_gate_confidence(0.65) - 0.35).abs() < 1e-3);
    }

    #[test]
    fn density_gate_boundary_mutations_caught() {
        // Values just below the upper plateau (1.0) must be in the linear region.
        let v = density_gate_confidence(0.999);
        assert!(v < 0.6 - 1e-4, "0.999 wps must be below plateau, got {v}");
        // Values just above the lower floor (0.3) must exceed it.
        let v = density_gate_confidence(0.301);
        assert!(v > 0.1 + 1e-4, "0.301 wps must exceed floor, got {v}");
    }

    #[test]
    fn match_advances_past_consumed_autosub_word() {
        // Two reference "a"s should match two different autosub "a"s — not the
        // same one twice. If pointer fails to advance, both ref words match idx 0
        // and pick up start_ms=100 instead of 100 and 200.
        let ref_words = vec!["a", "a"];
        let autosub = vec![
            AutosubWord {
                text: "a".into(),
                start_ms: 100,
            },
            AutosubWord {
                text: "a".into(),
                start_ms: 200,
            },
        ];
        let out = match_reference_to_autosub(&ref_words, &autosub, 10);
        assert_eq!(out[0].autosub_start_ms, Some(100));
        assert_eq!(
            out[1].autosub_start_ms,
            Some(200),
            "second 'a' must match the second autosub word, not the first"
        );
    }

    #[test]
    fn match_noise_reference_words_produce_none_without_consuming_autosub() {
        let ref_words = vec!["[music]", "hello"];
        let autosub = vec![AutosubWord {
            text: "hello".into(),
            start_ms: 500,
        }];
        let out = match_reference_to_autosub(&ref_words, &autosub, 10);
        assert_eq!(out[0].autosub_start_ms, None);
        assert_eq!(out[0].reference_text, "[music]");
        assert_eq!(
            out[1].autosub_start_ms,
            Some(500),
            "noise reference must not consume the autosub pointer"
        );
    }

    #[tokio::test]
    async fn can_provide_false_when_path_is_none() {
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: std::path::PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![CandidateText {
                source: "reference".into(),
                lines: vec!["Hello world".into()],
                has_timing: false,
                line_timings: None,
            }],
            autosub_json3: None,
            duration_ms: 180_000,
        };
        assert!(!AutoSubProvider.can_provide(&ctx).await);
    }

    #[tokio::test]
    async fn can_provide_false_when_path_missing() {
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: std::path::PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![CandidateText {
                source: "reference".into(),
                lines: vec!["Hello world".into()],
                has_timing: false,
                line_timings: None,
            }],
            autosub_json3: Some(std::path::PathBuf::from("/tmp/does_not_exist_xyz.json3")),
            duration_ms: 180_000,
        };
        assert!(!AutoSubProvider.can_provide(&ctx).await);
    }

    #[tokio::test]
    async fn can_provide_false_when_under_10_words() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.json3");
        tokio::fs::write(
            &path,
            r#"{"events":[{"tStartMs":0,"segs":[{"utf8":"hi"}]}]}"#,
        )
        .await
        .unwrap();
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: std::path::PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![CandidateText {
                source: "reference".into(),
                lines: vec!["Hello world".into()],
                has_timing: false,
                line_timings: None,
            }],
            autosub_json3: Some(path),
            duration_ms: 180_000,
        };
        assert!(!AutoSubProvider.can_provide(&ctx).await);
    }

    #[tokio::test]
    async fn can_provide_false_when_density_below_threshold() {
        // 20 words / 100s = 0.2 wps < 0.3 → fail
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.json3");
        let mut events = String::from("{\"events\":[");
        for i in 0..20 {
            if i > 0 {
                events.push(',');
            }
            events.push_str(&format!(
                "{{\"tStartMs\":{},\"segs\":[{{\"utf8\":\"w{}\"}}]}}",
                i * 5000,
                i
            ));
        }
        events.push_str("]}");
        tokio::fs::write(&path, events).await.unwrap();
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: std::path::PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![CandidateText {
                source: "reference".into(),
                lines: vec!["Hello world".into()],
                has_timing: false,
                line_timings: None,
            }],
            autosub_json3: Some(path),
            duration_ms: 100_000,
        };
        assert!(!AutoSubProvider.can_provide(&ctx).await);
    }

    #[tokio::test]
    async fn can_provide_true_when_dense_enough() {
        // 100 words / 100s = 1.0 wps → pass
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dense.json3");
        let mut events = String::from("{\"events\":[");
        for i in 0..100 {
            if i > 0 {
                events.push(',');
            }
            events.push_str(&format!(
                "{{\"tStartMs\":{},\"segs\":[{{\"utf8\":\"w{}\"}}]}}",
                i * 1000,
                i
            ));
        }
        events.push_str("]}");
        tokio::fs::write(&path, events).await.unwrap();
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: std::path::PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![CandidateText {
                source: "reference".into(),
                lines: vec!["Hello world".into()],
                has_timing: false,
                line_timings: None,
            }],
            autosub_json3: Some(path),
            duration_ms: 100_000,
        };
        assert!(AutoSubProvider.can_provide(&ctx).await);
    }

    #[tokio::test]
    async fn align_emits_matched_words_in_reference_line_structure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aligned.json3");
        tokio::fs::write(
            &path,
            r#"{"events":[
          {"tStartMs":1000,"segs":[{"utf8":"Hello","tOffsetMs":0},{"utf8":"world","tOffsetMs":200}]},
          {"tStartMs":2000,"segs":[{"utf8":"how","tOffsetMs":0},{"utf8":"are","tOffsetMs":200},{"utf8":"you","tOffsetMs":400}]}
        ]}"#,
        )
        .await
        .unwrap();
        let ctx = SongContext {
            video_id: "test".into(),
            audio_path: std::path::PathBuf::from("/tmp/test.flac"),
            clean_vocal_path: None,
            candidate_texts: vec![CandidateText {
                source: "reference".into(),
                lines: vec!["Hello world".into(), "how are you".into()],
                has_timing: true,
                line_timings: Some(vec![(1000, 2000), (2000, 3000)]),
            }],
            autosub_json3: Some(path),
            duration_ms: 5000, // 5 words / 5s = 1.0 wps
        };
        let result = AutoSubProvider.align(&ctx).await.unwrap();
        assert_eq!(result.provider_name, "autosub");
        assert_eq!(result.lines.len(), 2, "preserves reference line count");
        assert_eq!(result.lines[0].words.len(), 2);
        assert_eq!(result.lines[0].words[0].text, "Hello");
        assert_eq!(result.lines[0].words[0].start_ms, 1000);
        assert_eq!(result.lines[1].words.len(), 3);
        assert_eq!(result.lines[1].words[0].text, "how");
        assert_eq!(result.lines[1].words[0].start_ms, 2000);
    }
}
