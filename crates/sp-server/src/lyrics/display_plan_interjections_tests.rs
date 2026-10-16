//! Tests for #280's interjection rule, through `build_plan` (what the wall
//! and the Presenter get). The lines are the census's real ones (SNV,
//! 16.10.2026, design 6223125533) and the #217 fixtures' (design
//! 6223162067); the pins were derived with a scratch Python model of the
//! plan.

use sp_core::lyrics::LyricsLine;

use super::super::{DisplayProfile, build_plan};

fn sung(start_ms: u64, end_ms: u64, en: &str, sk: &str) -> LyricsLine {
    LyricsLine {
        start_ms,
        end_ms,
        en: en.to_string(),
        sk: Some(sk.to_string()),
        words: None,
    }
}

/// What the wall shows of one source line, as (EN, SK); `None` when the line
/// leaves the plan.
fn wall(en: &str, sk: Option<&str>) -> Option<(String, Option<String>)> {
    let line = LyricsLine {
        start_ms: 1_000,
        end_ms: 2_000,
        en: en.to_string(),
        sk: sk.map(str::to_string),
        words: None,
    };
    let plan = build_plan(&[line], DisplayProfile::Song);
    assert!(plan.len() <= 1, "{plan:?}");
    plan.into_iter().next().map(|d| (d.en, d.sk))
}

/// An expected [`wall`] result.
fn shows(en: &str, sk: Option<&str>) -> Option<(String, Option<String>)> {
    Some((en.to_string(), sk.map(str::to_string)))
}

// ── lines that are only interjections ──────────────────────────────────────

#[test]
fn a_line_of_only_interjections_leaves_the_plan() {
    for (en, sk) in [
        ("Oh.", "Och."),
        ("Yeah, yeah", "Áno, no"),
        ("Hey!", "Hej!"),
        ("Mm. Mm.", "Mm. Mm."),
        ("Na na na, hey", "Na na na, hej"),
    ] {
        assert_eq!(wall(en, Some(sk)), None, "{en}");
    }
}

#[test]
fn a_line_that_is_only_an_aside_interjection_leaves_the_plan() {
    // The asides go first ("(Oh)" → "Oh"), then the interjection rule.
    assert_eq!(wall("(Oh)", Some("(Och)")), None);
}

#[test]
fn a_line_with_no_english_is_never_left_out() {
    // A dub subtitle line groups by its Slovak; the English is the judge.
    assert_eq!(wall("", Some("Áno.")), shows("", Some("Áno.")));
}

#[test]
fn the_owners_line_shows_its_sentence_alone() {
    // Song 290 "I STILL BELIEVE IT" (16.10.2026): "One more time we say" +
    // "Oh." showed as "Ešte raz hovoríme Och.".
    let plan = build_plan(
        &[
            sung(
                130_000,
                132_400,
                "One more time we say",
                "Ešte raz hovoríme",
            ),
            sung(133_000, 133_600, "Oh.", "Och."),
        ],
        DisplayProfile::Song,
    );
    assert_eq!(plan.len(), 1, "{plan:?}");
    assert_eq!(plan[0].en, "One more time we say");
    assert_eq!(plan[0].sk.as_deref(), Some("Ešte raz hovoríme"));
    assert_eq!(plan[0].src_range, 0..1);
    // Now the last line: it leaves 3 s after its own sung end.
    assert_eq!((plan[0].show_ms, plan[0].hide_ms), (129_200, 135_400));
}

#[test]
fn the_line_before_a_left_out_line_holds_through_its_time() {
    let plan = build_plan(
        &[
            sung(
                10_000,
                12_000,
                "We lift your name.",
                "Vyvyšujeme tvoje meno.",
            ),
            sung(14_000, 15_000, "Yeah, yeah", "Áno, no"),
            sung(19_000, 21_000, "Holy is the Lord.", "Svätý je Pán."),
        ],
        DisplayProfile::Song,
    );
    let got: Vec<_> = plan
        .iter()
        .map(|d| (d.src_range.clone(), d.show_ms, d.hide_ms))
        .collect();
    assert_eq!(got, [(0..1, 9_200, 18_200), (2..3, 18_200, 24_000)]);
}

#[test]
fn a_left_out_line_can_open_an_instrumental_break() {
    // Without "Oh.", 9 s pass from "We lift your name." to "Holy is the
    // Lord.": over 8 s, so the wall blanks for it.
    let plan = build_plan(
        &[
            sung(
                10_000,
                12_000,
                "We lift your name.",
                "Vyvyšujeme tvoje meno.",
            ),
            sung(15_000, 15_500, "Oh.", "Och."),
            sung(21_000, 23_000, "Holy is the Lord.", "Svätý je Pán."),
        ],
        DisplayProfile::Song,
    );
    let got: Vec<_> = plan
        .iter()
        .map(|d| (d.src_range.clone(), d.show_ms, d.hide_ms))
        .collect();
    assert_eq!(got, [(0..1, 9_200, 15_000), (2..3, 20_200, 26_000)]);
}

#[test]
fn a_left_out_line_inside_a_sentence_is_in_its_range() {
    let plan = build_plan(
        &[
            sung(0, 1_000, "Holy, holy,", "Svätý, svätý,"),
            sung(1_200, 1_500, "Oh,", "Ó,"),
            sung(1_800, 3_000, "Lord God.", "Pán Boh."),
        ],
        DisplayProfile::Song,
    );
    assert_eq!(plan.len(), 1, "{plan:?}");
    assert_eq!(plan[0].en, "Holy, holy, Lord God.");
    assert_eq!(plan[0].sk.as_deref(), Some("Svätý, svätý, Pán Boh."));
    assert_eq!(plan[0].src_range, 0..3);
}

#[test]
fn a_left_out_first_line_is_in_no_range() {
    let plan = build_plan(
        &[
            sung(0, 500, "Oh.", "Och."),
            sung(2_000, 4_000, "We sing.", "Spievame."),
        ],
        DisplayProfile::Song,
    );
    assert_eq!(plan.len(), 1, "{plan:?}");
    assert_eq!(plan[0].src_range, 1..2);
    assert_eq!((plan[0].show_ms, plan[0].hide_ms), (1_200, 7_000));
}

#[test]
fn a_line_whose_end_run_went_ends_its_sentence() {
    // 221's "like you. Oh,": with "Oh," gone the line ends in "." and
    // closes its wall line, where the "," joined it to the next sentence.
    let plan = build_plan(
        &[
            sung(0, 2_000, "There's no one", "Nikto nie je"),
            sung(2_100, 4_000, "like you. Oh,", "ako ty. Ó,"),
            sung(4_200, 6_000, "There is none.", "Nie je nikto."),
        ],
        DisplayProfile::Song,
    );
    let got: Vec<_> = plan
        .iter()
        .map(|d| (d.src_range.clone(), d.en.as_str(), d.sk.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            (
                0..2,
                "There's no one like you.",
                Some("Nikto nie je ako ty.")
            ),
            (2..3, "There is none.", Some("Nie je nikto.")),
        ]
    );
}

// ── interjections at a line's edge ─────────────────────────────────────────

#[test]
fn a_leading_run_with_a_mark_goes_and_the_next_word_takes_its_capital() {
    assert_eq!(
        wall("Yeah, I'm free", Some("Áno, som slobodný")),
        shows("I'm free", Some("Som slobodný"))
    );
    assert_eq!(
        wall("Yeah, let's dance.", Some("Áno, poďme tancovať.")),
        shows("Let's dance.", Some("Poďme tancovať."))
    );
    // A line with no Slovak changes only its English.
    assert_eq!(wall("Oh, what a God.", None), shows("What a God.", None));
}

#[test]
fn a_lower_case_line_stays_lower_case() {
    assert_eq!(
        wall("oh, we sing.", Some("ó, my spievame.")),
        shows("we sing.", Some("my spievame."))
    );
}

#[test]
fn a_trailing_run_goes_when_a_mark_ends_the_word_before_it() {
    assert_eq!(
        wall("Say whoa oh. Yeah.", Some("Povedzte whoa oh. Áno.")),
        shows("Say whoa oh.", Some("Povedzte whoa oh."))
    );
    assert_eq!(
        wall("like you. Oh,", Some("ako ty. Ó,")),
        shows("like you.", Some("ako ty."))
    );
    assert_eq!(
        wall("like you. Oh", Some("ako ty. Ó")),
        shows("like you.", Some("ako ty."))
    );
}

#[test]
fn an_interjection_inside_a_line_stays() {
    let en = "'Cause the blood washed me clean, yeah, I'm free indeed";
    let sk = "Lebo krv ma obmyla dočista, áno, som naozaj slobodný";
    assert_eq!(wall(en, Some(sk)), shows(en, Some(sk)));
}

#[test]
fn an_edge_run_no_mark_separates_stays() {
    // A vocative, and the chant's own words.
    for (en, sk) in [
        ("Oh Lord, you're beautiful", "Ó Pane, si nádherný"),
        ("Say whoa oh.", "Povedzte whoa oh."),
    ] {
        assert_eq!(wall(en, Some(sk)), shows(en, Some(sk)), "{en}");
    }
}

#[test]
fn a_cut_that_would_leave_one_word_leaves_the_line_whole() {
    assert_eq!(
        wall("Oh, God.", Some("Ó, Bože.")),
        shows("Oh, God.", Some("Ó, Bože."))
    );
}

#[test]
fn yes_is_content() {
    assert_eq!(
        wall("Yes, Lord", Some("Áno, Pane")),
        shows("Yes, Lord", Some("Áno, Pane"))
    );
}

#[test]
fn the_longest_run_the_marks_allow_goes() {
    assert_eq!(
        wall("Oh, yeah, I'm free", Some("Ó, áno, som slobodný")),
        shows("I'm free", Some("Som slobodný"))
    );
    // Only "Oh," is separated: "oh Lord" is a vocative.
    assert_eq!(
        wall("Oh, oh Lord, you are good", Some("Ó, ó Pane, si dobrý")),
        shows("Oh Lord, you are good", Some("Ó Pane, si dobrý"))
    );
    assert_eq!(
        wall(
            "Oh, yeah, we sing, yeah, oh.",
            Some("Ó, áno, my spievame, áno, ó.")
        ),
        shows("We sing.", Some("My spievame."))
    );
}

#[test]
fn slovak_follows_an_english_cut_only() {
    // At most as many words as the English lost at that edge.
    assert_eq!(
        wall("Oh, I'm free", Some("Ó, áno, som slobodný")),
        shows("I'm free", Some("Áno, som slobodný"))
    );
    // Only at an edge the English lost words at.
    assert_eq!(
        wall("I'm free, yeah.", Some("Áno, som slobodný, áno.")),
        shows("I'm free.", Some("Áno, som slobodný."))
    );
    // Under the same separation rule.
    assert_eq!(
        wall("Yeah, I'm free", Some("Áno som slobodný")),
        shows("I'm free", Some("Áno som slobodný"))
    );
    // And never down to one word.
    assert_eq!(
        wall("Oh, we are free", Some("Ó, slobodní")),
        shows("We are free", Some("Ó, slobodní"))
    );
}

#[test]
fn a_sentence_end_survives_the_end_cut() {
    assert_eq!(
        wall("I'm free indeed, yeah.", Some("Som naozaj slobodný, áno.")),
        shows("I'm free indeed.", Some("Som naozaj slobodný."))
    );
    // The kept word's own sentence end stays.
    assert_eq!(
        wall("Is he worthy? Yeah.", Some("Je hoden? Áno.")),
        shows("Is he worthy?", Some("Je hoden?"))
    );
    // The mark is read past a closing quote.
    assert_eq!(
        wall("He said \"holy,\" yeah.", Some("Povedal „svätý,“ áno.")),
        shows("He said \"holy.\"", Some("Povedal „svätý.“"))
    );
    assert_eq!(
        wall("We sing… oh", Some("My spievame… ó")),
        shows("We sing…", Some("My spievame…"))
    );
}

#[test]
fn an_interjection_matches_in_any_case_with_the_marks_around_it() {
    assert_eq!(wall("OH-OH, WE SING", None), shows("WE SING", None));
}

#[test]
fn a_line_with_nothing_to_cut_is_shown_as_it_is() {
    // Not re-joined either: a stored spacing the plan never asked about.
    assert_eq!(
        wall("Holy  is the Lord", Some("Svätý  je Pán ")),
        shows("Holy  is the Lord", Some("Svätý  je Pán "))
    );
}
