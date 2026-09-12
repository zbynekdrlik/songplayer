//! Reference gate — RED test scaffold for issue #143 Part A (design
//! settled on #130's 2026-09-12 comment). The full module doc and
//! implementation land in the paired GREEN commit; this commit is TESTS
//! ONLY and does not compile until GREEN adds `AlignedLine`, `GateStats`,
//! `GateFailReason`, `GateVerdict`, `match_lines`, and `evaluate`.

use crate::lyrics::g35t_client::AsrWord;

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, start_ms: u64) -> AlignedLine {
        AlignedLine {
            text: text.to_string(),
            start_ms,
        }
    }

    fn word(text: &str, start_ms: u64, end_ms: u64) -> AsrWord {
        AsrWord {
            text: text.to_string(),
            start_ms,
            end_ms,
        }
    }

    /// Push one phrase's words into `words`, one word per 300ms starting
    /// at `start`.
    fn push_phrase(words: &mut Vec<AsrWord>, phrase: &str, start: u64) {
        let mut t = start;
        for tok in phrase.split_whitespace() {
            words.push(word(tok, t, t + 300));
            t += 300;
        }
    }

    #[test]
    fn perfect_agreement_passes_with_full_within_400_frac() {
        let lines = vec![
            line("one two three", 1000),
            line("four five six", 2000),
            line("seven eight nine", 3000),
            line("ten eleven twelve", 4000),
            line("thirteen fourteen fifteen", 5000),
        ];
        let mut words = Vec::new();
        for l in &lines {
            push_phrase(&mut words, &l.text, l.start_ms);
        }

        match evaluate(&lines, &words) {
            GateVerdict::Pass(stats) => {
                assert_eq!(stats.lines_total, 5);
                assert_eq!(stats.lines_matched, 5);
                assert_eq!(stats.matched_frac, 1.0);
                assert_eq!(stats.median_signed_ms, 0);
                assert_eq!(stats.within_400_frac, 1.0);
            }
            other => panic!("expected Pass, got {other:?}"),
        }
    }

    #[test]
    fn whole_song_22s_shift_fails_offset_despite_full_match() {
        // Forced alignment locked onto the wrong (later) repetition of the
        // whole song — every line's claimed start is 22s later than where
        // the words actually are. Text still matches perfectly (Coverage
        // and Agreement would both pass), but the median offset trips the
        // Offset gate.
        let texts = [
            "alpha beta gamma",
            "delta epsilon zeta",
            "eta theta iota",
            "kappa lambda mu",
            "nu xi omicron",
        ];
        let mut lines = Vec::new();
        let mut words = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            let asr_base = 1000 + i as u64 * 1000;
            lines.push(line(text, asr_base + 22_000));
            push_phrase(&mut words, text, asr_base);
        }

        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Offset,
                stats,
            } => {
                assert_eq!(stats.lines_matched, 5);
                assert_eq!(stats.median_signed_ms, -22_000);
            }
            other => panic!("expected Fail(Offset), got {other:?}"),
        }
    }

    #[test]
    fn too_many_unmatched_lines_fails_coverage() {
        // Only 2 of 5 lines' text appears anywhere in the ASR stream —
        // matched_frac = 0.4, well under the 0.60 floor.
        let lines = vec![
            line("apple banana cherry", 1000),
            line("this text is nowhere", 2000),
            line("date elderberry fig", 3000),
            line("also missing entirely", 4000),
            line("also not present here", 5000),
        ];
        let mut words = Vec::new();
        push_phrase(&mut words, "apple banana cherry", 1000);
        push_phrase(&mut words, "date elderberry fig", 3000);

        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Coverage,
                stats,
            } => {
                assert_eq!(stats.lines_total, 5);
                assert_eq!(stats.lines_matched, 2);
                assert!((stats.matched_frac - 0.4).abs() < 1e-9);
            }
            other => panic!("expected Fail(Coverage), got {other:?}"),
        }
    }

    #[test]
    fn zero_lines_fails_coverage() {
        let lines: Vec<AlignedLine> = vec![];
        let words: Vec<AsrWord> = vec![];
        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Coverage,
                stats,
            } => {
                assert_eq!(stats.lines_total, 0);
            }
            other => panic!("expected Fail(Coverage), got {other:?}"),
        }
    }

    #[test]
    fn half_of_matched_lines_900ms_off_fails_agreement() {
        // All 4 lines match (Coverage passes); the +900/-900 deltas cancel
        // to a median of 0 (Offset passes), but only 2 of 4 (50%) land
        // within 400ms — under the 70% Agreement floor.
        let lines = vec![
            line("alpha bravo charlie", 1000),
            line("delta echo foxtrot", 5000),
            line("golf hotel india", 9000),
            line("juliet kilo lima", 13000),
        ];
        let mut words = Vec::new();
        push_phrase(&mut words, "alpha bravo charlie", 1900); // +900
        push_phrase(&mut words, "delta echo foxtrot", 4100); // -900
        push_phrase(&mut words, "golf hotel india", 9000); // 0
        push_phrase(&mut words, "juliet kilo lima", 13000); // 0

        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Agreement,
                stats,
            } => {
                assert_eq!(stats.lines_matched, 4);
                assert_eq!(stats.median_signed_ms, 0);
                assert!((stats.within_400_frac - 0.5).abs() < 1e-9);
            }
            other => panic!("expected Fail(Agreement), got {other:?}"),
        }
    }

    #[test]
    fn repeated_chorus_lines_match_forward_never_backward() {
        let lines = vec![
            line("we lift you up", 1000),
            line("some unique verse line", 5000),
            line("we lift you up", 9000),
            line("another unique verse", 13000),
            line("we lift you up", 17000),
        ];
        let mut words = Vec::new();
        push_phrase(&mut words, "we lift you up", 1000);
        push_phrase(&mut words, "some unique verse line", 5000);
        push_phrase(&mut words, "we lift you up", 9000);
        push_phrase(&mut words, "another unique verse", 13000);
        push_phrase(&mut words, "we lift you up", 17000);

        let matches = match_lines(&lines, &words);
        let starts: Vec<u64> = matches
            .iter()
            .map(|m| m.expect("every line should match"))
            .collect();
        assert_eq!(starts, vec![1000, 5000, 9000, 13000, 17000]);
        for pair in starts.windows(2) {
            assert!(
                pair[1] > pair[0],
                "cursor must advance monotonically, never rebind backward: {starts:?}"
            );
        }
    }

    /// Real fixture: `gemini-3-5-transcribe_YbGFYaA0SbY.json`'s own
    /// `lines[].words[]` as the independent ASR word source, and that same
    /// file's own `lines[].text`/`start_ms` (optionally shifted) as the
    /// forced-alignment lines under test — a self-consistency check
    /// against real Gemini 3.5 Transcribe output, not synthetic data.
    fn load_fixture_lines_and_words() -> (Vec<AlignedLine>, Vec<AsrWord>) {
        let raw = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../eval/lyrics/reports/2026-09-12-raw/gemini-3-5-transcribe_YbGFYaA0SbY.json"
        ));
        let v: serde_json::Value = serde_json::from_str(raw).expect("fixture must be valid JSON");
        let lines_json = v["lines"].as_array().expect("fixture must have lines[]");

        let aligned_lines: Vec<AlignedLine> = lines_json
            .iter()
            .map(|l| AlignedLine {
                text: l["text"].as_str().expect("line.text").to_string(),
                start_ms: l["start_ms"].as_u64().expect("line.start_ms"),
            })
            .collect();

        let words: Vec<AsrWord> = lines_json
            .iter()
            .flat_map(|l| {
                l["words"]
                    .as_array()
                    .expect("line.words")
                    .iter()
                    .map(|w| AsrWord {
                        text: w["text"].as_str().expect("word.text").to_string(),
                        start_ms: w["start_ms"].as_u64().expect("word.start_ms"),
                        end_ms: w["end_ms"].as_u64().expect("word.end_ms"),
                    })
            })
            .collect();

        (aligned_lines, words)
    }

    #[test]
    fn real_fixture_unshifted_passes() {
        let (aligned_lines, words) = load_fixture_lines_and_words();
        match evaluate(&aligned_lines, &words) {
            GateVerdict::Pass(stats) => {
                assert!(stats.matched_frac >= MIN_MATCHED_FRAC);
                assert_eq!(stats.median_signed_ms, 0);
            }
            other => panic!("expected Pass on the unshifted real fixture, got {other:?}"),
        }
    }

    #[test]
    fn real_fixture_shifted_30s_fails_offset() {
        let (mut aligned_lines, words) = load_fixture_lines_and_words();
        for l in &mut aligned_lines {
            l.start_ms += 30_000;
        }
        match evaluate(&aligned_lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Offset,
                stats,
            } => {
                assert_eq!(stats.median_signed_ms, -30_000);
            }
            other => panic!("expected Fail(Offset) on the 30s-shifted fixture, got {other:?}"),
        }
    }
}
