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
