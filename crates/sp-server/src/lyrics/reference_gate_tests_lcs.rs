//! #144 first-week review (ROZHODNUTÉ, point 1): the reference → transcript
//! half matches lines through the same order-preserving word alignment the
//! sung half computes (`sung_coverage.rs`). A line is matched when at least
//! half of its words are on the alignment; its start is the sung start of its
//! first aligned word. The forward walk it replaces lost every line after one
//! misheard start: the 1-word fallback found that word much later in the
//! song and the cursor jumped there.
//!
//! Wired via `#[cfg(test)] #[path = "reference_gate_tests_lcs.rs"]`.

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

/// One phrase's words, one per 300 ms from `start`.
fn push_phrase(words: &mut Vec<AsrWord>, phrase: &str, start: u64) {
    let mut t = start;
    for tok in phrase.split_whitespace() {
        words.push(word(tok, t, t + 300));
        t += 300;
    }
}

/// The ASR heard "can" for "Can't"; "can't" is sung later in the song. The
/// forward walk bound the first line to that later "can't" and lost the
/// three lines between (`[60000, None, None, None]`). On the alignment the
/// first line holds 3 of its 4 words and starts at its first sung one.
#[test]
fn a_misheard_line_start_does_not_orphan_the_lines_after_it() {
    let lines = vec![
        line("Can't take my worship", 1_000),
        line("Lord you are holy", 4_000),
        line("we lift your name", 7_000),
        line("holy holy holy", 10_000),
    ];
    let mut words = Vec::new();
    push_phrase(&mut words, "can take my worship", 1_000);
    push_phrase(&mut words, "Lord you are holy", 4_000);
    push_phrase(&mut words, "we lift your name", 7_000);
    push_phrase(&mut words, "holy holy holy", 10_000);
    push_phrase(&mut words, "can't stop praising", 60_000);
    assert_eq!(
        match_lines(&lines, &words),
        vec![Some(1_300), Some(4_000), Some(7_000), Some(10_000)]
    );
}

/// Half of the line's words sung (its second half): matched, at the first
/// sung word. The forward walk needed the line's FIRST word.
#[test]
fn a_line_whose_second_half_is_sung_is_matched_at_its_first_sung_word() {
    let lines = vec![
        line("alpha bravo charlie delta", 1_000),
        line("echo foxtrot golf", 3_000),
    ];
    let mut words = Vec::new();
    push_phrase(&mut words, "charlie delta", 1_600);
    push_phrase(&mut words, "echo foxtrot golf", 3_000);
    assert_eq!(match_lines(&lines, &words), vec![Some(1_600), Some(3_000)]);
}

/// One of four words sung is under half: not matched. The forward walk took
/// a lone first word as the whole line.
#[test]
fn a_line_with_only_its_first_word_sung_is_not_matched() {
    let lines = vec![
        line("alpha bravo charlie delta", 1_000),
        line("echo foxtrot golf", 3_000),
    ];
    let mut words = Vec::new();
    push_phrase(&mut words, "alpha", 1_000);
    push_phrase(&mut words, "echo foxtrot golf", 3_000);
    assert_eq!(match_lines(&lines, &words), vec![None, Some(3_000)]);
}

/// An in-repo eval fixture: mtl's lines (2026-08-05) against the real Gemini
/// 3.5 Transcribe words (2026-09-12) of the same video.
macro_rules! fixture {
    ($yt:literal) => {
        fixture_from(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../eval/lyrics/reports/2026-08-05-aligner-raw/lyrics-alignment-mtl_",
                $yt,
                ".json"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../eval/lyrics/reports/2026-09-12-raw/gemini-3-5-transcribe_",
                $yt,
                ".json"
            )),
        )
    };
}

fn fixture_from(mtl: &str, g35t: &str) -> (Vec<AlignedLine>, Vec<AsrWord>) {
    let mtl: serde_json::Value = serde_json::from_str(mtl).expect("mtl fixture JSON");
    let g35t: serde_json::Value = serde_json::from_str(g35t).expect("g35t fixture JSON");
    let lines = mtl["lines"]
        .as_array()
        .expect("mtl lines[]")
        .iter()
        .map(|l| line(l["text"].as_str().unwrap(), l["start_ms"].as_u64().unwrap()))
        .collect();
    let words = g35t["lines"]
        .as_array()
        .expect("g35t lines[]")
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
    (lines, words)
}

/// `(lines_total, lines_matched, median_signed_ms, lines within 400 ms)` of a
/// passing verdict.
fn passing(verdict: GateVerdict) -> (usize, usize, i64, f64) {
    match verdict {
        GateVerdict::Pass(s) => (
            s.lines_total,
            s.lines_matched,
            s.median_signed_ms,
            s.within_400_frac,
        ),
        other => panic!("expected Pass, got {other:?}"),
    }
}

/// The three fixtures the review named: the forward walk failed them on
/// matched lines with medians of +102 / +313 / +444 s. On the alignment they
/// pass on time (pins from the scratch port of `evaluate`).
#[test]
fn the_eval_fixtures_the_forward_walk_lost_pass_on_time() {
    let (lines, words) = fixture!("KeZaADiRHVI");
    assert_eq!(passing(evaluate(&lines, &words)), (53, 44, 8, 35.0 / 44.0));
    let (lines, words) = fixture!("p74PDWAFk0A");
    assert_eq!(
        passing(evaluate(&lines, &words)),
        (162, 145, -27, 125.0 / 145.0)
    );
    let (lines, words) = fixture!("q5m09rqOoxE");
    assert_eq!(
        passing(evaluate(&lines, &words)),
        (214, 187, -46, 159.0 / 187.0)
    );
}

/// The correct rejections stay rejected: the poisoned fixture and the two
/// texts that were partial on 2026-08-05 (53 and 13 lines).
#[test]
fn the_eval_fixtures_with_poisoned_or_partial_texts_still_fail_coverage() {
    for (yt, (lines, words)) in [
        ("Xvm4_fWkXe8", fixture!("Xvm4_fWkXe8")),
        ("edZVnKxKEUU", fixture!("edZVnKxKEUU")),
        ("xPkg_vW4yE0", fixture!("xPkg_vW4yE0")),
    ] {
        match evaluate(&lines, &words) {
            GateVerdict::Fail {
                reason: GateFailReason::Coverage,
                ..
            } => {}
            other => panic!("{yt}: expected Fail(Coverage), got {other:?}"),
        }
    }
}
