//! Tests for #280's asides rule. The pins are song 361's real lines
//! ("Redeemed", Red Worship, Genius text), derived with a scratch Python
//! model of `drop_asides`.

use sp_core::lyrics::{LyricsLine, LyricsWord};

use super::super::{DisplayProfile, build_plan};
use super::{drop_asides, without_asides};

#[test]
fn the_asides_of_song_361_go_and_the_words_join_again() {
    let table = [
        (
            "Anybody, anybody, anybody? (Oh)",
            "Anybody, anybody, anybody?",
        ),
        (
            "'Cause only He could do it (Sing)",
            "'Cause only He could do it",
        ),
        (
            "Hallelu (Hallelujah), hallelujah (I've been redeemed)",
            "Hallelu, hallelujah",
        ),
        (
            "Hallelu (Hey, oh, hallelujah), hallelujah (I have been redeemed, yeah)",
            "Hallelu, hallelujah",
        ),
        ("I've been redeemed (Sing, \"Halle\")", "I've been redeemed"),
        (
            "Anybody been set free? (Oh; come on, y'all, hey)",
            "Anybody been set free?",
        ),
        (
            "Anybody been set free? (Anybody been redeemed? Anybody been set free?)",
            "Anybody been set free?",
        ),
        (
            "'Cause the blood washed me clean, yeah, I'm free indeed (Oh, I walked out the grave)",
            "'Cause the blood washed me clean, yeah, I'm free indeed",
        ),
        (
            "'Cause the blood washed me clean (Oh), yeah, I'm free indeed (Walked out, sing)",
            "'Cause the blood washed me clean, yeah, I'm free indeed",
        ),
    ];
    for (text, shown) in table {
        assert_eq!(drop_asides(text), shown, "{text}");
    }
}

#[test]
fn the_slovak_asides_go_by_the_same_rule() {
    let table = [
        ("Niekto, niekto, niekto? (Och)", "Niekto, niekto, niekto?"),
        (
            "Halelu (Hej, och, aleluja), aleluja (Som vykúpený, áno)",
            "Halelu, aleluja",
        ),
        (
            "Lebo krv ma obmyla dočista (Och), áno, som naozaj slobodný (Vyšiel som, spievaj)",
            "Lebo krv ma obmyla dočista, áno, som naozaj slobodný",
        ),
    ];
    for (text, shown) in table {
        assert_eq!(drop_asides(text), shown, "{text}");
    }
}

#[test]
fn a_line_that_is_only_asides_keeps_its_words_without_the_brackets() {
    assert_eq!(drop_asides("(Hallelujah)"), "Hallelujah");
    assert_eq!(drop_asides("(Sing, \"Halle\")"), "Sing, \"Halle\"");
    assert_eq!(drop_asides("( )"), "");
}

#[test]
fn nested_asides_go_whole() {
    assert_eq!(drop_asides("a (b (c) d) e"), "a e");
}

#[test]
fn parentheses_that_do_not_pair_up_leave_the_line_as_it_is() {
    assert_eq!(drop_asides("a (b"), "a (b");
    assert_eq!(drop_asides("a) b"), "a) b");
    assert_eq!(drop_asides("a (b) c)"), "a (b) c)");
}

#[test]
fn a_line_with_no_parenthesis_is_returned_as_it_is() {
    // Not re-joined either: a stored spacing the plan never asked about.
    assert_eq!(drop_asides("Hello , world"), "Hello , world");
    assert_eq!(drop_asides("  two  spaces "), "  two  spaces ");
    assert_eq!(drop_asides(""), "");
}

#[test]
fn the_words_on_both_sides_of_an_aside_never_run_together() {
    assert_eq!(drop_asides("word(aside)next"), "word next");
}

#[test]
fn a_mark_left_at_the_start_is_dropped_and_one_inside_is_glued() {
    assert_eq!(drop_asides("(Oh), yeah"), "yeah");
    assert_eq!(drop_asides("Halle (lu) ! jah (x) …"), "Halle! jah…");
}

#[test]
fn without_asides_keeps_times_and_words_and_reads_both_languages() {
    let words = vec![LyricsWord {
        text: "Anybody".to_string(),
        start_ms: 1_000,
        end_ms: 1_400,
    }];
    let line = LyricsLine {
        start_ms: 1_000,
        end_ms: 2_500,
        en: "Anybody? (Oh)".to_string(),
        sk: Some("Niekto? (Och)".to_string()),
        words: Some(words.clone()),
    };
    let shown = without_asides(&line);
    assert_eq!(shown.en, "Anybody?");
    assert_eq!(shown.sk.as_deref(), Some("Niekto?"));
    assert_eq!((shown.start_ms, shown.end_ms), (1_000, 2_500));
    assert_eq!(shown.words, Some(words));
    let no_sk = LyricsLine { sk: None, ..line };
    assert_eq!(without_asides(&no_sk).sk, None);
}

fn sung(start_ms: u64, end_ms: u64, en: &str, sk: &str) -> LyricsLine {
    LyricsLine {
        start_ms,
        end_ms,
        en: en.to_string(),
        sk: Some(sk.to_string()),
        words: None,
    }
}

#[test]
fn the_plan_shows_both_languages_without_their_asides() {
    let plan = build_plan(
        &[sung(1_000, 2_500, "Anybody? (Oh)", "Niekto? (Och)")],
        DisplayProfile::Song,
    );
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].en, "Anybody?");
    assert_eq!(plan[0].sk.as_deref(), Some("Niekto?"));
    assert_eq!(plan[0].src_range, 0..1);
}

#[test]
fn two_lines_that_fit_one_wall_line_only_without_their_asides_are_one() {
    // Raw: 70 + 1 + 29 chars, over MAX_CHARS, so two wall lines; without
    // the asides: 19 + 1 + 19 = 39 chars, one.
    let lines = [
        sung(
            0,
            2_000,
            "Hallelu (Hey, oh, hallelujah), hallelujah (I have been redeemed, yeah)",
            "",
        ),
        sung(2_500, 4_000, "I've been redeemed (You say).", ""),
    ];
    let plan = build_plan(&lines, DisplayProfile::Song);
    assert_eq!(plan.len(), 1, "{plan:?}");
    assert_eq!(plan[0].en, "Hallelu, hallelujah I've been redeemed.");
    assert_eq!(plan[0].src_range, 0..2);
}

#[test]
fn a_line_whose_aside_hid_its_question_mark_ends_its_sentence() {
    // Raw, `free? (Yeah)` ends in `h` (the `)` is read past), so both lines
    // form one sentence; without the aside it ends in `?`.
    let lines = [
        sung(0, 2_000, "Anybody been set free? (Yeah)", ""),
        sung(2_500, 4_000, "Next line.", ""),
    ];
    let plan = build_plan(&lines, DisplayProfile::Song);
    assert_eq!(plan.len(), 2, "{plan:?}");
    assert_eq!(plan[0].en, "Anybody been set free?");
    assert_eq!(plan[1].en, "Next line.");
}
