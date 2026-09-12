//! Gemini 3.5 Transcribe client — RED test scaffold for issue #143 Part A
//! (design settled on #130's 2026-09-12 comment). The full module doc and
//! implementation land in the paired GREEN commit; this commit is TESTS
//! ONLY and does not compile until GREEN adds `AsrWord`,
//! `transcribe_words`, `parse_offset_ms`, `words_from_response`, and
//! `gemini_keys_from_setting`.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn parse_offset_ms_parses_standard_and_bare_seconds() {
        assert_eq!(parse_offset_ms("5.200s"), Some(5200));
        assert_eq!(parse_offset_ms("9s"), Some(9000));
        assert_eq!(parse_offset_ms("0.05s"), Some(50));
    }

    #[test]
    fn parse_offset_ms_rejects_malformed_or_missing_suffix() {
        assert_eq!(parse_offset_ms("x"), None);
        assert_eq!(parse_offset_ms(""), None);
        assert_eq!(parse_offset_ms("5.2"), None);
        assert_eq!(parse_offset_ms("-1s"), None);
    }

    #[test]
    fn words_from_response_skips_malformed_and_non_word_annotations() {
        let response: Value = serde_json::json!({
            "steps": [
                {
                    "content": [
                        {
                            "annotations": [
                                {"type": "word_info", "text": "Nothing", "start_offset": "5.200s", "end_offset": "9s"},
                                {"type": "other", "text": "ignored", "start_offset": "1s", "end_offset": "2s"}
                            ]
                        }
                    ]
                },
                {
                    "content": [
                        {
                            "annotations": [
                                {"type": "word_info", "text": "compares", "start_offset": "9s", "end_offset": "9.6s"},
                                {"type": "word_info", "text": "", "start_offset": "9.6s", "end_offset": "10s"}
                            ]
                        }
                    ]
                }
            ]
        });

        let words = words_from_response(&response);
        assert_eq!(
            words.len(),
            2,
            "the non-word_info and empty-text entries must be skipped"
        );
        assert_eq!(words[0].text, "Nothing");
        assert_eq!(words[0].start_ms, 5200);
        assert_eq!(words[0].end_ms, 9000);
        assert_eq!(words[1].text, "compares");
        assert_eq!(words[1].start_ms, 9000);
        assert_eq!(words[1].end_ms, 9600);
    }

    #[test]
    fn words_from_response_empty_on_missing_steps() {
        let response: Value = serde_json::json!({"status": "completed"});
        assert!(words_from_response(&response).is_empty());
    }

    #[test]
    fn gemini_keys_from_setting_splits_trims_and_drops_empties() {
        assert_eq!(
            gemini_keys_from_setting(" key1 , key2,, key3 "),
            vec!["key1".to_string(), "key2".to_string(), "key3".to_string()]
        );
        assert_eq!(gemini_keys_from_setting(""), Vec::<String>::new());
        assert_eq!(
            gemini_keys_from_setting("onlyone"),
            vec!["onlyone".to_string()]
        );
    }
}
