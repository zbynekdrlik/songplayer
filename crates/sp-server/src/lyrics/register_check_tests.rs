//! #250: the register check's prompt, answer parsing and apply rules.

use sp_core::lyrics::{LyricsLine, LyricsTrack, LyricsWord, RegisterLine, RegisterVerdict};

use super::{
    Finding, REGISTER_CHECK_VERSION, apply, build_prompt, max_fixes, may_fix, numbered_lines,
    parse_response, read_answer, refused, unreadable,
};

fn line(en: &str, sk: Option<&str>) -> LyricsLine {
    LyricsLine {
        start_ms: 1_000,
        end_ms: 2_000,
        en: en.to_string(),
        sk: sk.map(str::to_string),
        words: None,
    }
}

/// "We Raise" as PP's base tier wrote it (11.10.2026), plus fine lines.
fn we_raise() -> LyricsTrack {
    LyricsTrack {
        version: 22,
        source: "gemini-3-5-transcribe".into(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines: vec![
            line("We ran and we ran", Some("Bežali sme a bežali")),
            line("I won't swell in the house", Some("Nenapuchnem v dome")),
            line("Holy is the Lord", Some("Svätý je Pán")),
            line("through endless more", Some("cez nekonečné viac")),
        ],
        register: None,
    }
}

fn fix(index: usize, en: &str, sk: &str) -> Finding {
    Finding::Fix {
        index,
        en: en.into(),
        sk: sk.into(),
    }
}

#[test]
fn the_prompt_numbers_each_line_with_its_slovak() {
    let numbered = numbered_lines(&we_raise());
    assert_eq!(
        numbered,
        "1: We ran and we ran || Bežali sme a bežali\n\
         2: I won't swell in the house || Nenapuchnem v dome\n\
         3: Holy is the Lord || Svätý je Pán\n\
         4: through endless more || cez nekonečné viac"
    );
    let prompt = build_prompt(&numbered);
    assert!(prompt.ends_with(&format!("\n\n{numbered}")));
    assert!(prompt.contains("\"N | corrected English | corrected Slovak\""));
    assert!(prompt.contains("output only NONE"));
    // The wording probed live on SNV (#250, 11.10.2026): only a misheard
    // biblical / devotional phrase or an indecent line, every other line left.
    assert!(prompt.contains(
        "Find ONLY (a) the lines where the recognition clearly misheard a \
         well-known biblical or devotional phrase, and (b) the lines whose English \
         or Slovak reads as vulgar, crude or indecent. Leave every other line \
         alone, even when it reads oddly. For each such line"
    ));
    assert!(!prompt.contains("  "), "no doubled space in the prompt");
}

#[test]
fn a_line_without_slovak_is_numbered_with_an_empty_rendering() {
    let mut track = we_raise();
    track.lines[0].sk = None;
    assert!(numbered_lines(&track).starts_with("1: We ran and we ran || \n2: "));
}

#[test]
fn none_means_every_line_reads_right() {
    assert_eq!(parse_response("NONE", 4), Some(vec![]));
    assert_eq!(parse_response("  none \n", 4), Some(vec![]));
}

#[test]
fn an_answer_with_neither_none_nor_a_finding_is_unreadable() {
    assert_eq!(parse_response("", 4), None);
    assert_eq!(parse_response("All lines look fine to me.", 4), None);
    assert_eq!(parse_response("2: I will dwell", 4), None);
}

#[test]
fn findings_are_read_one_per_line() {
    let answer = "2 | I will dwell in the house | Budem prebývať v dome\n\
                  4 | ? | ?\n\
                  1 | We raise and we raise | Dvíhame a dvíhame";
    assert_eq!(
        parse_response(answer, 4),
        Some(vec![
            fix(1, "I will dwell in the house", "Budem prebývať v dome"),
            Finding::Unsure { index: 3 },
            fix(0, "We raise and we raise", "Dvíhame a dvíhame"),
        ])
    );
}

#[test]
fn an_unknown_english_or_slovak_is_an_unsure_line() {
    assert_eq!(
        parse_response("1 | ? | Dvíhame", 4),
        Some(vec![Finding::Unsure { index: 0 }])
    );
    assert_eq!(
        parse_response("1 | We raise | ?", 4),
        Some(vec![Finding::Unsure { index: 0 }])
    );
    assert_eq!(
        parse_response("1 | We raise |", 4),
        Some(vec![Finding::Unsure { index: 0 }])
    );
    assert_eq!(
        parse_response("1 |  | Dvíhame", 4),
        Some(vec![Finding::Unsure { index: 0 }])
    );
}

#[test]
fn a_finding_outside_the_track_or_in_another_shape_is_ignored() {
    // Line 0, line 5 of a 4-line track, a 4th field, a missing field, a word.
    let answer = "0 | a | b\n5 | a | b\n2 | a | b | c\n3 | a\nx | a | b";
    assert_eq!(parse_response(answer, 4), None);
    // The last line of the track is in it.
    assert_eq!(parse_response("4 | a | b", 4), Some(vec![fix(3, "a", "b")]));
}

#[test]
fn a_line_given_twice_keeps_its_first_finding() {
    assert_eq!(
        parse_response("2 | a | b\n2 | c | d", 4),
        Some(vec![fix(1, "a", "b")])
    );
}

#[test]
fn a_fix_rewrites_the_line_and_keeps_its_old_text() {
    let mut track = we_raise();
    track.lines[1].words = Some(vec![LyricsWord {
        text: "swell".into(),
        start_ms: 1_200,
        end_ms: 1_400,
    }]);
    let check = apply(
        &mut track,
        vec![fix(1, "I will dwell in the house", "Budem prebývať v dome")],
    );
    assert_eq!(check.version, REGISTER_CHECK_VERSION);
    assert_eq!(check.verdict, RegisterVerdict::Fixed);
    assert_eq!(
        check.fixed,
        vec![RegisterLine {
            index: 1,
            en: "I won't swell in the house".into(),
            sk: Some("Nenapuchnem v dome".into()),
        }]
    );
    assert!(check.flagged.is_empty());
    let fixed = &track.lines[1];
    assert_eq!(fixed.en, "I will dwell in the house");
    assert_eq!(fixed.sk.as_deref(), Some("Budem prebývať v dome"));
    assert_eq!(
        (fixed.start_ms, fixed.end_ms),
        (1_000, 2_000),
        "timing kept"
    );
    assert_eq!(fixed.words, None, "the old words' timings are dropped");
    assert_eq!(
        track.lines[0].en, "We ran and we ran",
        "other lines untouched"
    );
}

#[test]
fn an_unsure_line_is_flagged_and_left_as_it_is() {
    let mut track = we_raise();
    let before = track.clone();
    let check = apply(&mut track, vec![Finding::Unsure { index: 3 }]);
    assert_eq!(check.verdict, RegisterVerdict::Flagged);
    assert_eq!(
        check.flagged,
        vec![RegisterLine {
            index: 3,
            en: "through endless more".into(),
            sk: Some("cez nekonečné viac".into()),
        }]
    );
    assert!(check.fixed.is_empty());
    assert_eq!(track, before);
}

#[test]
fn a_flag_wins_the_verdict_over_a_fix() {
    let mut track = we_raise();
    let check = apply(
        &mut track,
        vec![
            Finding::Unsure { index: 3 },
            fix(1, "I will dwell", "Budem prebývať"),
        ],
    );
    assert_eq!(check.verdict, RegisterVerdict::Flagged);
    assert_eq!(check.fixed.len(), 1);
    assert_eq!(check.flagged.len(), 1);
}

#[test]
fn no_finding_is_an_ok_track_left_as_it_is() {
    let mut track = we_raise();
    let before = track.clone();
    let check = apply(&mut track, vec![]);
    assert_eq!(check.verdict, RegisterVerdict::Ok);
    assert!(check.fixed.is_empty() && check.flagged.is_empty());
    assert_eq!(track, before);
}

#[test]
fn a_fix_equal_to_the_line_changes_nothing_and_is_not_listed() {
    let mut track = we_raise();
    let check = apply(&mut track, vec![fix(2, "Holy is the Lord", "Svätý je Pán")]);
    assert_eq!(check.verdict, RegisterVerdict::Ok);
    assert!(check.fixed.is_empty());
    // A new Slovak alone is a change.
    let check = apply(
        &mut track,
        vec![fix(2, "Holy is the Lord", "Svätý je Hospodin")],
    );
    assert_eq!(check.verdict, RegisterVerdict::Fixed);
    assert_eq!(track.lines[2].sk.as_deref(), Some("Svätý je Hospodin"));
    // A new English alone too.
    let check = apply(
        &mut track,
        vec![fix(2, "Holy, holy Lord", "Svätý je Hospodin")],
    );
    assert_eq!(check.verdict, RegisterVerdict::Fixed);
    assert_eq!(track.lines[2].en, "Holy, holy Lord");
}

#[test]
fn a_line_with_no_slovak_takes_the_fixes_slovak() {
    let mut track = we_raise();
    track.lines[2].sk = None;
    let check = apply(&mut track, vec![fix(2, "Holy is the Lord", "Svätý je Pán")]);
    assert_eq!(check.verdict, RegisterVerdict::Fixed);
    assert_eq!(track.lines[2].sk.as_deref(), Some("Svätý je Pán"));
}

#[test]
fn the_listed_lines_are_in_track_order() {
    let mut track = we_raise();
    let check = apply(
        &mut track,
        vec![
            fix(3, "forevermore", "naveky"),
            fix(0, "We raise", "Dvíhame"),
        ],
    );
    let fixed: Vec<usize> = check.fixed.iter().map(|l| l.index).collect();
    assert_eq!(fixed, [0, 3]);
    let mut track = we_raise();
    let check = apply(
        &mut track,
        vec![Finding::Unsure { index: 3 }, Finding::Unsure { index: 1 }],
    );
    let flagged: Vec<usize> = check.flagged.iter().map(|l| l.index).collect();
    assert_eq!(flagged, [1, 3]);
}

#[test]
fn at_most_a_quarter_of_the_lines_and_at_least_three_are_fixed() {
    assert_eq!(max_fixes(0), 3);
    assert_eq!(max_fixes(12), 3);
    assert_eq!(max_fixes(15), 3);
    assert_eq!(max_fixes(16), 4);
    assert_eq!(max_fixes(40), 10);
    assert_eq!(max_fixes(43), 10);
}

/// A 16-line track: `max_fixes` is 4.
fn sixteen() -> LyricsTrack {
    let mut track = we_raise();
    track.lines = (0..16)
        .map(|i| line(&format!("line {i}"), Some(&format!("riadok {i}"))))
        .collect();
    track
}

#[test]
fn up_to_the_bound_every_fix_is_applied() {
    let mut track = sixteen();
    let fixes = (0..4).map(|i| fix(i, "new", "nové")).collect();
    let check = apply(&mut track, fixes);
    assert_eq!(check.verdict, RegisterVerdict::Fixed);
    assert_eq!(check.fixed.len(), 4);
    assert!(track.lines[..4].iter().all(|l| l.en == "new"));
}

#[test]
fn more_fixes_than_the_bound_is_a_rewrite_and_flags_them_all() {
    let mut track = sixteen();
    let before = track.clone();
    let mut findings: Vec<Finding> = (0..5).map(|i| fix(i, "new", "nové")).collect();
    // An unsure line does not count toward the bound.
    findings.push(Finding::Unsure { index: 9 });
    let check = apply(&mut track, findings);
    assert_eq!(check.verdict, RegisterVerdict::Flagged);
    assert!(check.fixed.is_empty());
    let flagged: Vec<usize> = check.flagged.iter().map(|l| l.index).collect();
    assert_eq!(flagged, [0, 1, 2, 3, 4, 9]);
    assert_eq!(track, before, "a rewrite changes nothing");
}

#[test]
fn fixes_equal_to_their_lines_do_not_count_toward_the_bound() {
    let mut track = sixteen();
    // Four real fixes plus two that change nothing: not a rewrite.
    let mut findings: Vec<Finding> = (0..4).map(|i| fix(i, "new", "nové")).collect();
    findings.push(fix(5, "line 5", "riadok 5"));
    findings.push(fix(6, "line 6", "riadok 6"));
    let check = apply(&mut track, findings);
    assert_eq!(check.verdict, RegisterVerdict::Fixed);
    assert_eq!(check.fixed.len(), 4);
}

#[test]
fn a_refusal_is_recorded_and_changes_nothing() {
    let check = refused();
    assert_eq!(check.version, REGISTER_CHECK_VERSION);
    assert_eq!(check.verdict, RegisterVerdict::Refused);
    assert!(check.fixed.is_empty() && check.flagged.is_empty());
}

#[test]
fn an_answer_is_findings_a_refusal_or_an_error() {
    assert_eq!(read_answer("NONE", 4).unwrap(), Some(vec![]));
    assert_eq!(
        read_answer("2 | a | b", 4).unwrap(),
        Some(vec![fix(1, "a", "b")])
    );
    assert_eq!(read_answer("I can't help with that.", 4).unwrap(), None);
    let err = read_answer("Here is my review of the text", 4).unwrap_err();
    assert!(err.to_string().contains("Here is my review"));
    assert!(unreadable(&err), "an answer in another shape is told apart");
    assert!(!unreadable(&anyhow::anyhow!("the proxy answered 503")));
    assert!(
        unreadable(&err.context("the stale pass")),
        "through a context too"
    );
}

#[test]
fn the_check_round_trips_with_the_track() {
    let mut track = we_raise();
    track.register = Some(apply(
        &mut track.clone(),
        vec![
            fix(1, "I will dwell", "Budem prebývať"),
            Finding::Unsure { index: 3 },
        ],
    ));
    let json = serde_json::to_string(&track).unwrap();
    assert!(json.contains("\"verdict\":\"flagged\""));
    let back: LyricsTrack = serde_json::from_str(&json).unwrap();
    assert_eq!(back, track);
    // A track from before #250 reads with no check.
    let old = r#"{"version":22,"source":"x","lines":[]}"#;
    let old: LyricsTrack = serde_json::from_str(old).unwrap();
    assert_eq!(old.register, None);
    assert!(!serde_json::to_string(&old).unwrap().contains("register"));
}

#[test]
fn the_verdicts_have_their_stored_names() {
    assert_eq!(RegisterVerdict::Ok.as_str(), "ok");
    assert_eq!(RegisterVerdict::Fixed.as_str(), "fixed");
    assert_eq!(RegisterVerdict::Flagged.as_str(), "flagged");
    assert_eq!(RegisterVerdict::Refused.as_str(), "refused");
}

#[test]
fn none_with_a_full_stop_is_still_none() {
    assert_eq!(parse_response("NONE.", 4), Some(vec![]));
    assert_eq!(parse_response("None!", 4), Some(vec![]));
    assert_eq!(parse_response("NONE, all fine", 4), None);
}

#[test]
fn an_operators_text_or_a_star_text_is_never_rewritten() {
    let with_source = |source: &str| {
        let mut track = we_raise();
        track.source = source.into();
        track
    };
    assert!(may_fix(&with_source("gemini-3-5-transcribe")));
    assert!(may_fix(&with_source("lrclib")));
    assert!(!may_fix(&with_source("override")));
    assert!(!may_fix(&with_source("genius+mtl@rev1/g35t-ok")));
    // A ★ track's misheard line is flagged, its text left as it is.
    let mut track = with_source("genius+mtl@rev1/g35t-ok");
    let before = track.clone();
    let check = apply(&mut track, vec![fix(1, "I will dwell", "Budem prebývať")]);
    assert_eq!(check.verdict, RegisterVerdict::Flagged);
    assert!(check.fixed.is_empty());
    assert_eq!(check.flagged.len(), 1);
    assert_eq!(track, before);
    // An operator's text too.
    let mut track = with_source("override");
    let check = apply(&mut track, vec![fix(1, "I will dwell", "Budem prebývať")]);
    assert_eq!(check.verdict, RegisterVerdict::Flagged);
}
