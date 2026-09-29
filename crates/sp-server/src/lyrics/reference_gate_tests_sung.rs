//! #144 — the reference gate must also check what is SUNG: a reference text
//! whose every line is found in the transcript on time can still leave long
//! sung stretches uncovered (a partial YouTube-description lyric), and mtl
//! then stretches its lines over them (song 286: "I believe in the Gospel"
//! held 48 s on the wall). Wired into `reference_gate.rs` as a sibling
//! `#[path]` test module.

use super::*;
use crate::lyrics::g35t_client::AsrWord;

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

/// Push one phrase's words, one word per 300 ms starting at `start`.
fn push_phrase(words: &mut Vec<AsrWord>, phrase: &str, start: u64) {
    let mut t = start;
    for tok in phrase.split_whitespace() {
        words.push(word(tok, t, t + 300));
        t += 300;
    }
}

/// Five six-word lines, each found in the transcript exactly at its own
/// start (so `matched_frac` 1.0, median offset 0, agreement 1.0), then a
/// slow sung stretch the text does not hold: 8 other words spread from
/// 30 000 ms to 56 000 ms (one every 3 600 ms, each 800 ms long). 30 of 38
/// sung words are covered (0.79, above the fraction floor), but the
/// uncovered stretch spans 30 000 → 56 000 = 26 000 ms, over the 25 s
/// maximum.
#[test]
fn a_text_missing_a_long_sung_stretch_fails_coverage() {
    let texts = [
        "alpha bravo charlie delta echo foxtrot",
        "golf hotel india juliet kilo lima",
        "mike november oscar papa quebec romeo",
        "sierra tango uniform victor whiskey xray",
        "yankee zulu amber bronze copper dune",
    ];
    let mut lines = Vec::new();
    let mut words = Vec::new();
    for (i, t) in texts.iter().enumerate() {
        let start = 1_000 + i as u64 * 5_000;
        lines.push(line(t, start));
        push_phrase(&mut words, t, start);
    }
    let unsung = [
        "ember", "flint", "grove", "harbor", "island", "jade", "karst", "lagoon",
    ];
    for (i, w) in unsung.iter().enumerate() {
        let s = 30_000 + i as u64 * 3_600;
        words.push(word(w, s, s + 800));
    }
    // The last uncovered word is 30 000 + 7 × 3 600 = 55 200 → ends 56 000.
    assert_eq!(words.last().unwrap().end_ms, 56_000);

    match evaluate(&lines, &words) {
        GateVerdict::Fail {
            reason: GateFailReason::Coverage,
            ..
        } => {}
        other => panic!("a 26 s uncovered sung stretch must fail Coverage, got {other:?}"),
    }
}

/// Ten three-word lines, each found on time, but every line is followed by
/// a four-word sung phrase the text does not hold (1.2 s each, so no single
/// uncovered run comes near 25 s). Covered 30 of 70 sung words = 0.4286,
/// under the 0.55 floor: the text covers less than half of what is sung.
#[test]
fn a_text_covering_under_half_of_what_is_sung_fails_coverage() {
    let mut lines = Vec::new();
    let mut words = Vec::new();
    for i in 0..10u64 {
        let start = 1_000 + i * 4_000;
        let covered = format!("line{i}a line{i}b line{i}c");
        lines.push(line(&covered, start));
        push_phrase(&mut words, &covered, start);
        let unsung = format!("gap{i}a gap{i}b gap{i}c gap{i}d");
        push_phrase(&mut words, &unsung, start + 1_000);
    }
    assert_eq!(words.len(), 70);

    match evaluate(&lines, &words) {
        GateVerdict::Fail {
            reason: GateFailReason::Coverage,
            ..
        } => {}
        other => panic!("a text covering 30 of 70 sung words must fail Coverage, got {other:?}"),
    }
}

/// Real Gemini 3.5 Transcribe output (`gemini-3-5-transcribe_YbGFYaA0SbY`,
/// 76 lines, 400 words): its own first 25 lines at their own starts are a
/// partial lyric — every line is found on time, exactly like the description
/// text of song 286. The text covers 105 of the 400 sung words (0.2625) and
/// leaves 198 300 ms of singing uncovered: Coverage.
#[test]
fn real_fixture_first_third_of_the_lyric_fails_coverage() {
    let raw = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../eval/lyrics/reports/2026-09-12-raw/gemini-3-5-transcribe_YbGFYaA0SbY.json"
    ));
    let v: serde_json::Value = serde_json::from_str(raw).expect("fixture must be valid JSON");
    let lines_json = v["lines"].as_array().expect("fixture must have lines[]");
    let all_lines: Vec<AlignedLine> = lines_json
        .iter()
        .map(|l| line(l["text"].as_str().unwrap(), l["start_ms"].as_u64().unwrap()))
        .collect();
    let words: Vec<AsrWord> = lines_json
        .iter()
        .flat_map(|l| l["words"].as_array().unwrap().iter())
        .map(|w| {
            word(
                w["text"].as_str().unwrap(),
                w["start_ms"].as_u64().unwrap(),
                w["end_ms"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!((all_lines.len(), words.len()), (76, 400));

    match evaluate(&all_lines[..25], &words) {
        GateVerdict::Fail {
            reason: GateFailReason::Coverage,
            ..
        } => {}
        other => panic!("the first third of the lyric must fail Coverage, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// GREEN (#144): the new stats, the boundaries and the measured distribution.
// ---------------------------------------------------------------------------

/// The five six-word lines of `a_text_missing_a_long_sung_stretch_fails_coverage`
/// at 1 000 + 5 000·i, followed by `unsung` words at the given spans.
fn five_lines_then(unsung: &[(&str, u64, u64)]) -> (Vec<AlignedLine>, Vec<AsrWord>) {
    let texts = [
        "alpha bravo charlie delta echo foxtrot",
        "golf hotel india juliet kilo lima",
        "mike november oscar papa quebec romeo",
        "sierra tango uniform victor whiskey xray",
        "yankee zulu amber bronze copper dune",
    ];
    let mut lines = Vec::new();
    let mut words = Vec::new();
    for (i, t) in texts.iter().enumerate() {
        let start = 1_000 + i as u64 * 5_000;
        lines.push(line(t, start));
        push_phrase(&mut words, t, start);
    }
    for &(w, s, e) in unsung {
        words.push(word(w, s, e));
    }
    (lines, words)
}

#[test]
fn the_sung_numbers_are_in_the_gate_stats() {
    let unsung: Vec<(&str, u64, u64)> = [
        "ember", "flint", "grove", "harbor", "island", "jade", "karst", "lagoon",
    ]
    .iter()
    .enumerate()
    .map(|(i, w)| (*w, 30_000 + i as u64 * 3_600, 30_800 + i as u64 * 3_600))
    .collect();
    let (lines, words) = five_lines_then(&unsung);
    match evaluate(&lines, &words) {
        GateVerdict::Fail {
            reason: GateFailReason::Coverage,
            stats,
        } => {
            assert_eq!(stats.lines_matched, 5);
            assert_eq!(stats.matched_frac, 1.0);
            assert_eq!(stats.sung_words, 38);
            assert_eq!(stats.sung_covered_frac, 30.0 / 38.0);
            assert_eq!(stats.max_uncovered_sung_ms, 26_000);
        }
        other => panic!("expected Fail(Coverage), got {other:?}"),
    }
}

/// Two uncovered words spanning EXACTLY 25 000 ms pass (the maximum is
/// inclusive); one millisecond more fails Coverage.
#[test]
fn an_uncovered_stretch_exactly_at_the_maximum_passes() {
    let (lines, words) = five_lines_then(&[("ember", 30_000, 30_800), ("flint", 54_200, 55_000)]);
    match evaluate(&lines, &words) {
        GateVerdict::Pass(stats) => {
            assert_eq!(stats.max_uncovered_sung_ms, MAX_UNCOVERED_SUNG_MS);
            assert_eq!(stats.sung_covered_frac, 30.0 / 32.0);
        }
        other => panic!("a 25 000 ms stretch must pass, got {other:?}"),
    }

    let (lines, words) = five_lines_then(&[("ember", 30_000, 30_800), ("flint", 54_200, 55_001)]);
    match evaluate(&lines, &words) {
        GateVerdict::Fail {
            reason: GateFailReason::Coverage,
            stats,
        } => assert_eq!(stats.max_uncovered_sung_ms, 25_001),
        other => panic!("a 25 001 ms stretch must fail Coverage, got {other:?}"),
    }
}

/// Eleven one-word lines on time, nine short uncovered words between them:
/// 11 of 20 sung words = EXACTLY the 0.55 floor, which passes.
#[test]
fn a_covered_share_exactly_at_the_floor_passes() {
    let mut lines = Vec::new();
    let mut words = Vec::new();
    for i in 0..11u64 {
        let start = 1_000 + i * 2_000;
        let covered = format!("sung{i}");
        lines.push(line(&covered, start));
        words.push(word(&covered, start, start + 300));
        if i < 9 {
            words.push(word(&format!("extra{i}"), start + 500, start + 800));
        }
    }
    assert_eq!(words.len(), 20);
    match evaluate(&lines, &words) {
        GateVerdict::Pass(stats) => {
            assert_eq!(stats.sung_covered_frac, MIN_SUNG_COVERED_FRAC);
            assert_eq!(stats.max_uncovered_sung_ms, 300);
        }
        other => panic!("exactly 0.55 of what is sung must pass, got {other:?}"),
    }
}

/// A complete text the ASR partly mishears ("angel" for "hands") and pads
/// ("oh") still covers what is sung: 20 of 22 words, gaps of 300 ms.
#[test]
fn a_complete_text_with_misheard_words_passes() {
    let texts = [
        "we raise our hands up",
        "we give you all the praise",
        "nothing will stop us singing",
        "your holy spirit is here",
    ];
    let heard = [
        "we raise our angel up",
        "we give you all the praise",
        "nothing will stop us oh singing",
        "your holy spirit is here",
    ];
    let mut lines = Vec::new();
    let mut words = Vec::new();
    for (i, (t, h)) in texts.iter().zip(heard.iter()).enumerate() {
        let start = 1_000 + i as u64 * 4_000;
        lines.push(line(t, start));
        push_phrase(&mut words, h, start);
    }
    match evaluate(&lines, &words) {
        GateVerdict::Pass(stats) => {
            assert_eq!(stats.sung_words, 22);
            assert_eq!(stats.sung_covered_frac, 20.0 / 22.0);
            assert_eq!(stats.max_uncovered_sung_ms, 300);
        }
        other => panic!("a complete text must pass, got {other:?}"),
    }
}

/// The distribution measured on #144 (issue comment 5899043518), as
/// `(sung_covered_frac, max_uncovered_sung_ms)` of real texts against real
/// transcripts. Complete texts must pass the thresholds; the texts that held
/// one line on the wall over other singing must fail.
#[test]
fn the_thresholds_separate_the_measured_catalog() {
    use crate::lyrics::sung_coverage::SungCoverage;
    let complete = [
        (0.64, 7_000),   // eval gold jUnyHptnsRo (multi-language), the lowest share
        (0.73, 20_600),  // eval gold h-A1Tzkjsi4, the longest complete run
        (0.794, 14_100), // ★ 206 5JW87KKDTcU (lrclib) vs g35t
        (0.786, 16_300), // ★ 19 8kGxcXOAaCQ (description) vs WhisperX
        (0.878, 8_700),  // ★ 61 fS61hANi_60 (yt_subs) vs WhisperX
        (0.977, 800),    // ★ 86 yHO1bEnmjzg (description) vs WhisperX
    ];
    let incomplete = [
        (0.581, 48_200),  // 286 WL1ivzWbQGI: "I believe in the Gospel" held 48.1 s
        (0.628, 34_900),  // 270 ksxV-G8AB4o: one line held 38.5 s
        (0.730, 41_400),  // 77 duQLhle37MU: "Still You for me" held 91 s
        (0.325, 80_600),  // 41 HZLdKRGMGRE
        (0.333, 168_200), // 54 lfkdhaPeGtQ
        (0.565, 98_300),  // 43 7qJhOrHps80
        (0.413, 61_600),  // 49 XaoW1zn1DU0
        (0.470, 65_100),  // 134 bRkMBiNrAMk
        (0.42, 208_900),  // eval gold zVpDFHJtc_U cut to its first 35 %
    ];
    let cov = |(covered_frac, max_uncovered_ms): (f64, u64)| SungCoverage {
        sung_words: 300,
        covered_frac,
        max_uncovered_ms,
    };
    for m in complete {
        assert!(
            covers_what_is_sung(&cov(m)),
            "complete text {m:?} must pass"
        );
    }
    for m in incomplete {
        assert!(
            !covers_what_is_sung(&cov(m)),
            "partial text {m:?} must fail"
        );
    }
}
