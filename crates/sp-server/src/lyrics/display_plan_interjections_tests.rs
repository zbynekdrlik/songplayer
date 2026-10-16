//! Tests for #280's interjection rule, through `build_plan` (what the wall
//! and the Presenter get). The lines are the census's real ones (SNV,
//! 16.10.2026, design 6223125533) and the #217 fixtures' (design
//! 6223162067), plus cases built around their shapes; the pins were derived
//! with a scratch Python model of the plan.

use std::ops::Range;

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
    assert_eq!(wall("Oh (yeah)", Some("Ó (áno)")), None);
}

#[test]
fn a_line_whose_words_outside_its_asides_are_only_interjections_shows_them_all() {
    // Review round 1: "Oh (Hallelujah)" lost its "Oh" with the asides rule
    // and then the whole line with this one, while "(Hallelujah)" alone
    // shows "Hallelujah": the wall never blanks a sung aside. Such a line is
    // read with only its brackets gone, by the same rules.
    assert_eq!(
        wall("Oh (Hallelujah)", Some("Ó (Aleluja)")),
        shows("Oh Hallelujah", Some("Ó Aleluja"))
    );
    assert_eq!(
        wall("Yeah, (I'm free)", Some("Áno, (som slobodný)")),
        shows("I'm free", Some("Som slobodný"))
    );
}

#[test]
fn a_line_with_no_english_is_never_left_out() {
    // A sung line with no English groups by its Slovak; the English is the
    // judge, and an empty one judges nothing.
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
    // The left-out line's sentence end stays: it goes back to the line
    // before (the wall and the Presenter strip a line's final mark).
    assert_eq!(plan[0].en, "One more time we say.");
    assert_eq!(plan[0].sk.as_deref(), Some("Ešte raz hovoríme."));
    assert_eq!(plan[0].src_range, 0..1);
    // Now the last line: it leaves 3 s after its own sung end.
    assert_eq!((plan[0].show_ms, plan[0].hide_ms), (129_200, 135_400));
}

/// A wall line as (source lines, show, hide, EN, SK).
type WallLine = (Range<usize>, u64, u64, String, Option<String>);

/// Every wall line of `lines` (the Song profile).
fn wall_lines(lines: &[LyricsLine]) -> Vec<WallLine> {
    build_plan(lines, DisplayProfile::Song)
        .into_iter()
        .map(|d| (d.src_range, d.show_ms, d.hide_ms, d.en, d.sk))
        .collect()
}

/// An expected wall line.
fn wall_line(range: Range<usize>, show: u64, hide: u64, en: &str, sk: &str) -> WallLine {
    (range, show, hide, en.to_string(), Some(sk.to_string()))
}

#[test]
fn the_sentence_after_a_left_out_sentence_end_is_its_own_wall_line() {
    // Review round 1: with "Oh." left out, "One more time we say" ran into
    // the next sentence and lit it 5 s before it was sung.
    let lines = [
        sung(
            130_000,
            132_400,
            "One more time we say",
            "Ešte raz hovoríme",
        ),
        sung(133_000, 133_600, "Oh.", "Och."),
        sung(134_200, 136_500, "I still believe it.", "Stále tomu verím."),
    ];
    assert_eq!(
        wall_lines(&lines),
        [
            wall_line(
                0..1,
                129_200,
                133_900,
                "One more time we say.",
                "Ešte raz hovoríme."
            ),
            wall_line(
                2..3,
                133_900,
                139_500,
                "I still believe it.",
                "Stále tomu verím."
            ),
        ]
    );
    // The same with a "Yeah." in the middle of a chorus.
    let lines = [
        sung(10_000, 12_000, "We lift your name", "Vyvyšujeme tvoje meno"),
        sung(12_300, 12_800, "Yeah.", "Áno."),
        sung(
            13_000,
            15_000,
            "Holy is the Lord forever.",
            "Svätý je Pán naveky.",
        ),
    ];
    assert_eq!(
        wall_lines(&lines),
        [
            wall_line(
                0..1,
                9_200,
                13_000,
                "We lift your name.",
                "Vyvyšujeme tvoje meno."
            ),
            wall_line(
                2..3,
                13_000,
                18_000,
                "Holy is the Lord forever.",
                "Svätý je Pán naveky."
            ),
        ]
    );
}

#[test]
fn a_left_out_sentence_end_replaces_the_soft_end_before_it() {
    let lines = [
        sung(0, 1_500, "Holy, holy, holy,", "Svätý, svätý, svätý,"),
        sung(1_700, 2_000, "Oh!", "Och!"),
        sung(2_400, 4_000, "Lord God Almighty.", "Pán Boh Všemohúci."),
    ];
    assert_eq!(
        wall_lines(&lines),
        [
            wall_line(0..1, 0, 2_400, "Holy, holy, holy!", "Svätý, svätý, svätý!"),
            wall_line(
                2..3,
                2_400,
                7_000,
                "Lord God Almighty.",
                "Pán Boh Všemohúci."
            ),
        ]
    );
}

#[test]
fn a_line_that_ends_a_sentence_keeps_its_own_end() {
    // Nothing goes back to a line that ends a sentence, nor to an empty
    // English, nor from a left-out line with no sentence end.
    let lines = [
        sung(0, 1_000, "No one higher than you.", "Nikto vyšší ako ty."),
        sung(1_200, 1_500, "Oh!", "Och!"),
        sung(4_000, 5_000, "", "Áno."),
        sung(5_200, 5_500, "Yeah.", "Áno."),
        sung(8_000, 9_000, "We sing", "Spievame"),
        sung(9_200, 9_500, "Oh,", "Ó,"),
    ];
    let texts: Vec<_> = wall_lines(&lines)
        .into_iter()
        .map(|(range, _, _, en, sk)| (range, en, sk))
        .collect();
    let line =
        |range: Range<usize>, en: &str, sk: &str| (range, en.to_string(), Some(sk.to_string()));
    assert_eq!(
        texts,
        [
            line(0..1, "No one higher than you.", "Nikto vyšší ako ty."),
            line(2..3, "", "Áno."),
            line(4..5, "We sing", "Spievame"),
        ]
    );
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
    // The shape of 221's "like you. Oh,": with "Oh," gone the line ends in
    // "." and closes its wall line, where the "," joined it to the next
    // sentence. (In 221 itself the 52-char limit already split there.)
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

#[test]
fn a_dub_line_keeps_its_words_whatever_its_english() {
    // Review round 2: a dub line's English is the whole sentences matched to
    // it by time (#184 H5), not its translation, so a "Yeah." there must not
    // take the dub's own Slovak subtitle off the wall.
    let lines = [
        sung(0, 3_000, "Oh, we praise him.", "Ó, chválime ho."),
        sung(3_000, 5_000, "", "Áno."),
        sung(
            5_000,
            6_000,
            "Yeah.",
            "Áno, a preto ho chválime celým srdcom.",
        ),
    ];
    let got: Vec<WallLine> = build_plan(&lines, DisplayProfile::Speech)
        .into_iter()
        .map(|d| (d.src_range, d.show_ms, d.hide_ms, d.en, d.sk))
        .collect();
    assert_eq!(
        got,
        [
            wall_line(0..1, 0, 3_000, "Oh, we praise him.", "Ó, chválime ho."),
            wall_line(1..2, 3_000, 5_000, "", "Áno."),
            wall_line(
                2..3,
                5_000,
                9_000,
                "Yeah.",
                "Áno, a preto ho chválime celým srdcom."
            ),
        ]
    );
}

/// A wall line's (source lines, EN, SK).
type WallText = (Range<usize>, String, Option<String>);

/// The (source lines, EN, SK) of every wall line of `lines` (the Song
/// profile).
fn wall_texts(lines: &[LyricsLine]) -> Vec<WallText> {
    wall_lines(lines)
        .into_iter()
        .map(|(range, _, _, en, sk)| (range, en, sk))
        .collect()
}

/// An expected (source lines, EN, SK).
fn texts(range: Range<usize>, en: &str, sk: &str) -> WallText {
    (range, en.to_string(), Some(sk.to_string()))
}

#[test]
fn a_capital_left_out_at_a_sentence_start_goes_to_the_next_line() {
    // Follow-up: What A God's "Oh," (after "…what a God.") left
    // "what a God, what a God." starting lower-case on the wall.
    let lines = [
        sung(0, 2_000, "We sing to you.", "Spievame ti."),
        sung(2_500, 3_000, "Oh,", "Ó,"),
        sung(3_200, 5_000, "what a God, what a God.", "aký Boh, aký Boh."),
    ];
    assert_eq!(
        wall_texts(&lines),
        [
            texts(0..1, "We sing to you.", "Spievame ti."),
            texts(2..3, "What a God, what a God.", "Aký Boh, aký Boh."),
        ]
    );
    // The first line of a track begins a sentence too.
    let lines = [
        sung(0, 500, "Oh,", "Ó,"),
        sung(700, 2_000, "what a God.", "aký Boh."),
    ];
    assert_eq!(wall_texts(&lines), [texts(1..2, "What a God.", "Aký Boh.")]);
    // Each language by its own capital.
    let lines = [
        sung(0, 500, "Oh,", "ó,"),
        sung(700, 2_000, "what a God.", "aký Boh."),
    ];
    assert_eq!(wall_texts(&lines), [texts(1..2, "What a God.", "aký Boh.")]);
    // A kept line's sentence end is read after its own cut: "like you. Oh,"
    // ends in "." once its "Oh," is gone, so the next "Oh," begins one.
    let lines = [
        sung(
            0,
            1_500,
            "There is no one like you. Oh,",
            "Nikto nie je ako ty. Ó,",
        ),
        sung(1_700, 2_000, "Oh,", "Ó,"),
        sung(2_200, 3_500, "what a God.", "aký Boh."),
    ];
    assert_eq!(
        wall_texts(&lines),
        [
            texts(0..1, "There is no one like you.", "Nikto nie je ako ty."),
            texts(2..3, "What a God.", "Aký Boh."),
        ]
    );
    // Through a lower-case left-out line that goes on the same sentence.
    let lines = [
        sung(0, 1_000, "We sing.", "Spievame."),
        sung(1_200, 1_500, "Oh,", "Ó,"),
        sung(1_600, 1_900, "yeah,", "áno,"),
        sung(2_000, 3_000, "what a God.", "aký Boh."),
    ];
    assert_eq!(
        wall_texts(&lines),
        [
            texts(0..1, "We sing.", "Spievame."),
            texts(3..4, "What a God.", "Aký Boh."),
        ]
    );
}

#[test]
fn a_capital_left_out_inside_a_sentence_or_with_its_sentence_stays_there() {
    // Inside a sentence ("We sing" ends in no mark): the line goes on in
    // lower case, as `you were you're better.` does.
    let lines = [
        sung(0, 1_000, "We sing", "Spievame"),
        sung(1_200, 1_500, "Oh,", "Ó,"),
        sung(1_700, 3_000, "what a God.", "aký Boh."),
    ];
    assert_eq!(
        wall_texts(&lines),
        [texts(0..3, "We sing what a God.", "Spievame aký Boh.")]
    );
    // A left-out line that ends its own sentence takes its capital with it.
    let lines = [
        sung(0, 1_000, "We sing.", "Spievame."),
        sung(1_200, 1_500, "Oh.", "Ó."),
        sung(1_700, 3_000, "what a God.", "aký Boh."),
    ];
    assert_eq!(
        wall_texts(&lines),
        [
            texts(0..1, "We sing.", "Spievame."),
            texts(2..3, "what a God.", "aký Boh."),
        ]
    );
}

#[test]
fn a_word_that_starts_with_a_digit_takes_no_capital() {
    // Review round 4: the capital went to the first LETTER, inside the word
    // ("3Rd", "10,000 Reasons"). Only a first char that is a letter is
    // raised.
    assert_eq!(
        wall("Oh, 3rd day He rose", Some("Ó, 3. dňa vstal")),
        shows("3rd day He rose", Some("3. dňa vstal"))
    );
    let lines = [
        sung(0, 500, "Oh,", "Ó,"),
        sung(
            700,
            2_000,
            "10,000 reasons to sing.",
            "10 000 dôvodov spievať.",
        ),
    ];
    assert_eq!(
        wall_texts(&lines),
        [texts(
            1..2,
            "10,000 reasons to sing.",
            "10 000 dôvodov spievať."
        )]
    );
}

#[test]
fn a_lone_mark_is_glued_to_the_word_before_it() {
    // Follow-up: a token made only of `, . ; : ! ? …` stopped a run
    // (`Oh , yeah` showed `Oh ,`); it belongs to the word before it, as
    // the asides join reads it.
    assert_eq!(
        wall("Oh , yeah, I'm free", Some("Ó , áno, som slobodný")),
        shows("I'm free", Some("Som slobodný"))
    );
    assert_eq!(
        wall("We sing , oh.", Some("Spievame , ó.")),
        shows("We sing.", Some("Spievame."))
    );
    assert_eq!(wall("Oh , oh", Some("Ó , ó")), None);
    // Nothing to cut: the line as it is.
    assert_eq!(
        wall("Holy , holy Lord", Some("Svätý , svätý Pane")),
        shows("Holy , holy Lord", Some("Svätý , svätý Pane"))
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
    // A Slovak line of only interjections under an English end cut: its
    // first word is never part of the end run, so one word stays.
    assert_eq!(
        wall("We sing, oh, yeah.", Some("Áno, áno.")),
        shows("We sing.", Some("Áno."))
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
    // At the end too, at most as many words as the English lost there.
    assert_eq!(
        wall("I'm free, yeah.", Some("Som slobodný, áno, áno.")),
        shows("I'm free.", Some("Som slobodný, áno."))
    );
    // Under the same separation rule.
    assert_eq!(
        wall("Yeah, I'm free", Some("Áno som slobodný")),
        shows("I'm free", Some("Áno som slobodný"))
    );
    // Down to one word (review round 2: the wall shows the Slovak, and the
    // English cut already showed that edge to be separate).
    assert_eq!(
        wall("Oh, we are free", Some("Ó, slobodní")),
        shows("We are free", Some("Slobodní"))
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
