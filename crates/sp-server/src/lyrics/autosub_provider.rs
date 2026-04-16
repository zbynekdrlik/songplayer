//! YouTube auto-sub transfer alignment provider.
//!
//! Pulls word timestamps from yt-dlp's json3 caption format and matches them
//! against the orchestrator's selected reference text using the sequential
//! forward-walk matcher ported from `scripts/experiments/autosub_drift.py`.
//!
//! Density gate neutralizes worship-fast songs where YouTube ASR collapses:
//! densities below 0.3 words/sec fail `can_provide`, so the merge layer only
//! receives autosub results when they're likely to contribute signal.

use std::collections::HashSet;

/// A single word from the json3 auto-sub stream.
#[derive(Debug, Clone, PartialEq)]
pub struct AutosubWord {
    pub text: String,
    pub start_ms: u64,
}

/// Normalize a word for matching: lowercase, strip `[^\w]`, drop noise tokens.
/// Returns empty string for noise/empty/whitespace input.
pub fn normalize_word(s: &str) -> String {
    const NOISE: &[&str] = &["[music]", ">>", "[applause]", "[laughter]"];
    let trimmed = s.trim().to_lowercase();
    if trimmed.is_empty() || NOISE.iter().any(|n| trimmed == *n) {
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
    let noise: HashSet<&str> = ["[music]", ">>", "[applause]", "[laughter]"]
        .into_iter()
        .collect();
    out.retain(|w| !noise.contains(w.text.to_lowercase().as_str()));
    Ok(out)
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
}
