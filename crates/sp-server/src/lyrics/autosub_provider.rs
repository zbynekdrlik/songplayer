//! YouTube auto-sub transfer alignment provider.
//!
//! Pulls word timestamps from yt-dlp's json3 caption format and matches them
//! against the orchestrator's selected reference text using the sequential
//! forward-walk matcher ported from `scripts/experiments/autosub_drift.py`.
//!
//! Density gate neutralizes worship-fast songs where YouTube ASR collapses:
//! densities below 0.3 words/sec fail `can_provide`, so the merge layer only
//! receives autosub results when they're likely to contribute signal.

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
}
