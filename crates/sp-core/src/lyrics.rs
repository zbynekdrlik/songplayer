use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LyricsWord {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LyricsLine {
    pub start_ms: u64,
    pub end_ms: u64,
    pub en: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub words: Option<Vec<LyricsWord>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LyricsTrack {
    pub version: u32,
    pub source: String,
    #[serde(default)]
    pub language_source: String,
    #[serde(default)]
    pub language_translation: String,
    pub lines: Vec<LyricsLine>,
}

impl LyricsTrack {
    /// Find the line active at `position_ms`.
    /// Returns `(index, &line)` where `line.start_ms <= position_ms < line.end_ms`.
    pub fn line_at(&self, position_ms: u64) -> Option<(usize, &LyricsLine)> {
        self.lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.start_ms <= position_ms && position_ms < line.end_ms)
    }

    /// Find the index of the active word within `line` at `position_ms`.
    /// Searches in reverse for the last word where `w.start_ms <= position_ms`.
    /// Returns `None` if the line has no words or position is before the first word.
    pub fn word_index_at(&self, line: &LyricsLine, position_ms: u64) -> Option<usize> {
        let words = line.words.as_ref()?;
        words
            .iter()
            .enumerate()
            .rev()
            .find(|(_, w)| w.start_ms <= position_ms)
            .map(|(idx, _)| idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_track() -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "genius".into(),
            language_source: "en".into(),
            language_translation: "sk".into(),
            lines: vec![
                LyricsLine {
                    start_ms: 1000,
                    end_ms: 3000,
                    en: "Hello world".into(),
                    sk: Some("Ahoj svet".into()),
                    words: Some(vec![
                        LyricsWord {
                            text: "Hello".into(),
                            start_ms: 1000,
                            end_ms: 1800,
                        },
                        LyricsWord {
                            text: "world".into(),
                            start_ms: 2000,
                            end_ms: 3000,
                        },
                    ]),
                },
                LyricsLine {
                    start_ms: 3000,
                    end_ms: 5000,
                    en: "Goodbye".into(),
                    sk: None,
                    words: None,
                },
            ],
        }
    }

    // ---- serde roundtrip with full data ----

    #[test]
    fn serde_roundtrip_full() {
        let track = sample_track();
        let json = serde_json::to_string(&track).unwrap();
        let back: LyricsTrack = serde_json::from_str(&json).unwrap();
        assert_eq!(track, back);
    }

    // ---- serde without sk/words (skip_serializing_if) ----

    #[test]
    fn serde_skips_none_sk_and_words() {
        let line = LyricsLine {
            start_ms: 0,
            end_ms: 1000,
            en: "Test line".into(),
            sk: None,
            words: None,
        };
        let json = serde_json::to_string(&line).unwrap();
        // sk and words must not appear in serialized output when None
        assert!(!json.contains("\"sk\""), "sk should be skipped when None");
        assert!(
            !json.contains("\"words\""),
            "words should be skipped when None"
        );
    }

    #[test]
    fn serde_includes_sk_and_words_when_present() {
        let line = LyricsLine {
            start_ms: 0,
            end_ms: 1000,
            en: "Test line".into(),
            sk: Some("Testovacia riadka".into()),
            words: Some(vec![LyricsWord {
                text: "Test".into(),
                start_ms: 0,
                end_ms: 500,
            }]),
        };
        let json = serde_json::to_string(&line).unwrap();
        assert!(json.contains("\"sk\""));
        assert!(json.contains("\"words\""));
    }

    #[test]
    fn serde_roundtrip_without_optional_fields() {
        let track = LyricsTrack {
            version: 2,
            source: "lrclib".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 500,
                end_ms: 2000,
                en: "Simple line".into(),
                sk: None,
                words: None,
            }],
        };
        let json = serde_json::to_string(&track).unwrap();
        let back: LyricsTrack = serde_json::from_str(&json).unwrap();
        assert_eq!(track, back);
    }

    #[test]
    fn serde_default_language_fields_on_deserialize() {
        // language_source and language_translation use #[serde(default)]
        // so they deserialize as empty string when absent
        let json = r#"{"version":1,"source":"test","lines":[]}"#;
        let track: LyricsTrack = serde_json::from_str(json).unwrap();
        assert_eq!(track.language_source, "");
        assert_eq!(track.language_translation, "");
    }

    // ---- line_at finds correct line ----

    #[test]
    fn line_at_finds_first_line() {
        let track = sample_track();
        let result = track.line_at(1500);
        assert!(result.is_some());
        let (idx, line) = result.unwrap();
        assert_eq!(idx, 0);
        assert_eq!(line.en, "Hello world");
    }

    #[test]
    fn line_at_finds_second_line() {
        let track = sample_track();
        let result = track.line_at(4000);
        assert!(result.is_some());
        let (idx, line) = result.unwrap();
        assert_eq!(idx, 1);
        assert_eq!(line.en, "Goodbye");
    }

    // ---- line_at returns None outside range ----

    #[test]
    fn line_at_returns_none_before_first_line() {
        let track = sample_track();
        assert!(track.line_at(0).is_none());
        assert!(track.line_at(999).is_none());
    }

    #[test]
    fn line_at_returns_none_after_last_line() {
        let track = sample_track();
        assert!(track.line_at(5000).is_none());
        assert!(track.line_at(9999).is_none());
    }

    #[test]
    fn line_at_returns_none_on_empty_track() {
        let track = LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: String::new(),
            language_translation: String::new(),
            lines: vec![],
        };
        assert!(track.line_at(1000).is_none());
    }

    // ---- line_at boundary (start inclusive, end exclusive) ----

    #[test]
    fn line_at_start_inclusive() {
        let track = sample_track();
        // start_ms = 1000 should match
        let result = track.line_at(1000);
        assert!(result.is_some());
        let (idx, _) = result.unwrap();
        assert_eq!(idx, 0);
    }

    #[test]
    fn line_at_end_exclusive() {
        let track = sample_track();
        // end_ms of first line = 3000 = start_ms of second line
        // position 3000 should match second line (start inclusive), not first (end exclusive)
        let result = track.line_at(3000);
        assert!(result.is_some());
        let (idx, _) = result.unwrap();
        assert_eq!(idx, 1);
    }

    #[test]
    fn line_at_one_before_end_still_matches() {
        let track = sample_track();
        // 2999 < 3000 (end of first line) — should still match first line
        let result = track.line_at(2999);
        assert!(result.is_some());
        let (idx, _) = result.unwrap();
        assert_eq!(idx, 0);
    }

    // ---- word_index_at finds active word ----

    #[test]
    fn word_index_at_first_word() {
        let track = sample_track();
        let line = &track.lines[0];
        let idx = track.word_index_at(line, 1000);
        assert_eq!(idx, Some(0));
    }

    #[test]
    fn word_index_at_second_word() {
        let track = sample_track();
        let line = &track.lines[0];
        let idx = track.word_index_at(line, 2000);
        assert_eq!(idx, Some(1));
    }

    #[test]
    fn word_index_at_mid_first_word() {
        let track = sample_track();
        let line = &track.lines[0];
        let idx = track.word_index_at(line, 1500);
        assert_eq!(idx, Some(0));
    }

    #[test]
    fn word_index_at_between_words_returns_last_started() {
        // Between first word end (1800) and second word start (2000)
        // The last word whose start_ms <= 1900 is word 0 (start=1000)
        let track = sample_track();
        let line = &track.lines[0];
        let idx = track.word_index_at(line, 1900);
        assert_eq!(idx, Some(0));
    }

    // ---- word_index_at returns None without words ----

    #[test]
    fn word_index_at_returns_none_without_words() {
        let track = sample_track();
        let line = &track.lines[1]; // words: None
        assert!(track.word_index_at(line, 3500).is_none());
    }

    #[test]
    fn word_index_at_returns_none_empty_words() {
        let track = sample_track();
        let line = LyricsLine {
            start_ms: 0,
            end_ms: 1000,
            en: "Empty".into(),
            sk: None,
            words: Some(vec![]),
        };
        assert!(track.word_index_at(&line, 500).is_none());
    }

    // ---- word_index_at returns None before first word ----

    #[test]
    fn word_index_at_returns_none_before_first_word() {
        let track = sample_track();
        let line = &track.lines[0]; // first word starts at 1000
        // position 999 is before the first word
        assert!(track.word_index_at(line, 999).is_none());
    }
}
