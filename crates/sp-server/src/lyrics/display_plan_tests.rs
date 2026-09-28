//! Tests for the #217 LED-wall display plan: synthetic cases with exact
//! boundaries. The stored-lyrics fixtures are in
//! `display_plan_fixture_tests.rs`. The expected values were derived by hand
//! and cross-checked against a line-by-line reference model of `build_plan`.

use std::ops::Range;

use sp_core::lyrics::LyricsLine;

use super::{
    DisplayLine, DisplayPlan, DisplayProfile, GROUP_MAX_SPAN_MS, HOLD_TAIL_MS, LEAD_MAX_MS,
    LONG_GAP_MS, MAX_CHARS, MIN_VISIBLE_MS, SUSTAIN_MARGIN_MS, build_plan, join_sk, join_text,
};

fn line(start_ms: u64, end_ms: u64, en: &str, sk: &str) -> LyricsLine {
    LyricsLine {
        start_ms,
        end_ms,
        en: en.to_string(),
        sk: Some(sk.to_string()),
        words: None,
    }
}

/// A line with English text and an empty Slovak one.
fn en(start_ms: u64, end_ms: u64, text: &str) -> LyricsLine {
    line(start_ms, end_ms, text, "")
}

/// A dub subtitle line: Slovak text, no English.
fn sk(start_ms: u64, end_ms: u64, text: &str) -> LyricsLine {
    line(start_ms, end_ms, "", text)
}

/// The plan of sung lyrics (the Song profile).
fn song_plan(lines: &[LyricsLine]) -> Vec<DisplayLine> {
    build_plan(lines, DisplayProfile::Song)
}

/// [`DisplayPlan`] of sung lyrics (the Song profile).
fn song_display_plan(lines: &[LyricsLine]) -> DisplayPlan {
    DisplayPlan::build(lines, DisplayProfile::Song)
}

fn texts(plan: &[DisplayLine]) -> Vec<&str> {
    plan.iter().map(|d| d.en.as_str()).collect()
}

fn ranges(plan: &[DisplayLine]) -> Vec<Range<usize>> {
    plan.iter().map(|d| d.src_range.clone()).collect()
}

/// The source lines of a plan that must be ONE wall line. (A one-range array
/// literal would trip `clippy::single_range_in_vec_init`.)
fn single(plan: &[DisplayLine]) -> Range<usize> {
    assert_eq!(plan.len(), 1, "one wall line, got {:?}", texts(plan));
    plan[0].src_range.clone()
}

fn spans(plan: &[DisplayLine]) -> Vec<(u64, u64)> {
    plan.iter().map(|d| (d.show_ms, d.hide_ms)).collect()
}

fn index_at(plan: &DisplayPlan, position_ms: u64) -> Option<usize> {
    plan.at(position_ms).map(|(i, _)| i)
}

// ── constants ──────────────────────────────────────────────────────────────

#[test]
fn constants_match_the_design_record() {
    assert_eq!(LEAD_MAX_MS, 800);
    assert_eq!(SUSTAIN_MARGIN_MS, 1_500);
    assert_eq!(MIN_VISIBLE_MS, 1_200);
    assert_eq!(LONG_GAP_MS, 8_000);
    assert_eq!(HOLD_TAIL_MS, 3_000);
    assert_eq!(MAX_CHARS, 72);
    assert_eq!(GROUP_MAX_SPAN_MS, 6_500);
}

// ── grouping by sentence ───────────────────────────────────────────────────

/// Video 335 at 99.8–113.6 s, where the owner saw the wall switch
/// mid-sentence (28.9.2026). The fragment/merge plan showed "And now his
/// kingdom comes. Now his will be", then "done. Lift up your banners and
/// practice your praise." 1.5 s before "done." was sung.
#[test]
fn a_display_line_is_one_whole_sentence() {
    let plan = song_plan(&[
        line(
            99_800,
            103_941,
            "And now his kingdom comes.",
            "A teraz prichádza jeho kráľovstvo.",
        ),
        line(103_941, 106_200, "Now his will be", "Teraz sa deje jeho"),
        line(108_600, 109_168, "done.", "vôľa."),
        line(109_168, 110_306, "Lift up your", "Zdvihnite svoje"),
        line(110_306, 112_354, "banners and practice", "zástavy a cvičte"),
        line(112_354, 113_605, "your praise.", "svoju chválu."),
    ]);
    assert_eq!(
        texts(&plan),
        [
            "And now his kingdom comes.",
            "Now his will be done.",
            "Lift up your banners and practice your praise."
        ]
    );
    assert_eq!(ranges(&plan), [0..1, 1..3, 3..6]);
    assert_eq!(plan[1].sk.as_deref(), Some("Teraz sa deje jeho vôľa."));
    assert_eq!(
        plan[2].sk.as_deref(),
        Some("Zdvihnite svoje zástavy a cvičte svoju chválu.")
    );
    // Each sentence follows the previous one without a pause, so none leads:
    // each shows exactly when it starts being sung.
    assert_eq!(
        spans(&plan),
        [(99_000, 103_941), (103_941, 109_168), (109_168, 116_605)]
    );
}

#[test]
fn every_sentence_mark_ends_a_display_line() {
    // `. ! ? …`, also before closing quotes and brackets, the Slovak „…“,
    // ‚…‘ and »…« included (dub lines group by their SK).
    for text in [
        "Glory to God.",
        "Glory to God!",
        "Glory to God?",
        "Glory to God…",
        "Glory to God.\"",
        "Glory to God!”",
        "Glory to God?’",
        "Glory to God…»",
        "Glory to God.)",
        "Glory to God!]",
        "Glory to God.'",
        "Sláva Bohu.“",
        "Sláva Bohu.‘",
        "Sláva Bohu!«",
    ] {
        let plan = song_plan(&[en(0, 1_000, text), en(1_000, 2_000, "we sing")]);
        assert_eq!(ranges(&plan), [0..1, 1..2], "{text:?} ends a sentence");
    }
}

#[test]
fn a_line_without_a_sentence_mark_runs_on_into_the_next() {
    for text in [
        "Glory to God",
        "Glory to God,",
        "Glory to God;",
        "Glory to God:",
        "Glory to God—",
        "Glory to God-",
        "Glory to God–",
        "Glory to God\"",
        "Glory to God)",
    ] {
        let plan = song_plan(&[en(0, 1_000, text), en(1_000, 2_000, "we sing.")]);
        assert_eq!(single(&plan), 0..2, "{text:?} does not end a sentence");
        assert_eq!(plan[0].en, format!("{text} we sing."));
    }
}

#[test]
fn a_long_sentence_splits_after_its_last_soft_end() {
    // 32 + 1 + 17 + 1 + 45 chars do not fit in 72. When the middle line ends
    // in a soft end (`, ; : —`), it is the last one, so the split comes after
    // it.
    let first = "Lift up your hands, oh ye gates,";
    let last = "you everlasting doors, and the king of glory.";
    let sentence = |middle: &str| {
        song_plan(&[
            en(0, 1_000, first),
            en(1_000, 2_000, middle),
            en(2_000, 3_000, last),
        ])
    };
    for mark in [",", ";", ":", "—"] {
        let plan = sentence(format!("and be lifted up{mark}").as_str());
        assert_eq!(ranges(&plan), [0..2, 2..3], "{mark:?} is a soft end");
    }
    // Otherwise the last soft end is "gates,", and the middle line joins the
    // rest of the sentence (17 + 1 + 45 chars fit).
    for mark in ["", "-", "–"] {
        let plan = sentence(format!("and be lifted up{mark}").as_str());
        assert_eq!(ranges(&plan), [0..1, 1..3], "{mark:?} is no soft end");
    }
}

#[test]
fn a_long_sentence_with_no_soft_end_splits_before_the_line_that_overflows() {
    // 35 + 1 + 36 = 72 chars fit; one char more does not. Chars, not bytes.
    let first = "č".repeat(35);
    let at_72 = song_plan(&[
        en(0, 1_000, &first),
        en(1_000, 2_000, &format!("{}.", "ž".repeat(35))),
    ]);
    assert_eq!(single(&at_72), 0..2);
    assert_eq!(at_72[0].en.chars().count(), MAX_CHARS);
    let at_73 = song_plan(&[
        en(0, 1_000, &first),
        en(1_000, 2_000, &format!("{}.", "ž".repeat(36))),
    ]);
    assert_eq!(ranges(&at_73), [0..1, 1..2]);
}

#[test]
fn a_sentence_sung_over_6500_ms_splits_at_its_last_soft_end() {
    let sentence = |last_start: u64| {
        song_plan(&[
            en(0, 1_000, "Shout all ye people,"),
            en(1_000, 2_000, "shout it out"),
            en(3_000, 4_000, "and dance"),
            en(last_start, last_start + 1_000, "through the town."),
        ])
    };
    // The last line starts exactly 6500 ms after the first: one wall line.
    assert_eq!(single(&sentence(6_500)), 0..4);
    // At 6501 ms it splits after "people,", and the rest stays together.
    assert_eq!(ranges(&sentence(6_501)), [0..1, 1..4]);
}

#[test]
fn what_is_left_after_a_split_must_fit_too() {
    // "forever." starts 7600 ms after "Glory," and 7100 ms after "to God".
    // After the split at "Glory," the rest still spans over 6500 ms, so
    // "to God" stands alone.
    let plan = song_plan(&[
        en(0, 500, "Glory,"),
        en(500, 1_000, "to God"),
        en(7_600, 8_600, "forever."),
    ]);
    assert_eq!(ranges(&plan), [0..1, 1..2, 2..3]);
    // From 7000 ms the rest spans exactly 6500 ms and stays together.
    let plan = song_plan(&[
        en(0, 500, "Glory,"),
        en(500, 1_000, "to God"),
        en(7_000, 8_000, "forever."),
    ]);
    assert_eq!(ranges(&plan), [0..1, 1..3]);
}

#[test]
fn an_instrumental_break_ends_the_display_line_before_it() {
    // "forever." starts 8001 ms after "to God" is sung. The break closes
    // "Glory, to God" whole, and it leaves the wall 3 s after its end.
    let plan = song_plan(&[
        en(0, 500, "Glory,"),
        en(500, 1_000, "to God"),
        en(9_001, 10_000, "forever."),
    ]);
    assert_eq!(ranges(&plan), [0..2, 2..3]);
    assert_eq!(spans(&plan), [(0, 4_000), (8_201, 13_000)]);
    // A gap of exactly 8000 ms is no break. The 9000 ms span splits the
    // sentence at its soft end instead, and the rest again.
    let plan = song_plan(&[
        en(0, 500, "Glory,"),
        en(500, 1_000, "to God"),
        en(9_000, 10_000, "forever."),
    ]);
    assert_eq!(ranges(&plan), [0..1, 1..2, 2..3]);
}

#[test]
fn a_line_with_no_english_is_grouped_by_its_slovak() {
    // Dub subtitle lines have no EN; their SK is the text they group by.
    let plan = build_plan(
        &[
            sk(0, 500, "Áno"),
            sk(500, 1_000, "amen."),
            sk(1_000, 2_000, "Boh je dobrý."),
        ],
        DisplayProfile::Speech,
    );
    assert_eq!(ranges(&plan), [0..2, 2..3]);
    assert_eq!(plan[0].sk.as_deref(), Some("Áno amen."));
    assert_eq!(plan[0].en, "");
    // The 72-char limit reads the SK too: 40 + 1 + 42 chars do not fit.
    let text = "Boh nás miluje viac než si predstavíme a";
    let plan = build_plan(
        &[sk(0, 2_000, text), sk(2_000, 4_000, &format!("{text} b"))],
        DisplayProfile::Speech,
    );
    assert_eq!(ranges(&plan), [0..1, 1..2]);
}

#[test]
fn a_dub_subtitle_track_never_collapses_into_one_giant_sk_line() {
    // Dub subtitle tracks (`gemini-live-translate`, #182/#184) load into the
    // same LyricsState as songs. Their lines touch, carry no EN, and each
    // holds a whole SK sentence, so each is its own wall line.
    let text = "Boh nás miluje viac, než si dokážeme predstaviť, a volá nás k sebe.";
    let lines: Vec<LyricsLine> = (0..10u64)
        .map(|k| sk(1_000 + 4_000 * k, 5_000 + 4_000 * k, text))
        .collect();
    let plan = build_plan(&lines, DisplayProfile::Speech);
    assert_eq!(plan.len(), 10);
    assert!(plan.iter().all(|d| d.sk.as_deref() == Some(text)));
}

#[test]
fn join_text_puts_one_space_between_non_empty_texts() {
    assert_eq!(
        join_text("What a God,", "what a God."),
        "What a God, what a God."
    );
    assert_eq!(join_text(" a ", " b "), "a b");
    assert_eq!(join_text("", "b"), "b");
    assert_eq!(join_text("a", "  "), "a");
}

#[test]
fn join_sk_joins_what_is_present() {
    assert_eq!(
        join_sk(Some("Aký Boh,".into()), Some("aký Boh.".into())),
        Some("Aký Boh, aký Boh.".to_string())
    );
    assert_eq!(join_sk(Some("a".into()), None), Some("a".to_string()));
    assert_eq!(join_sk(None, Some("b".into())), Some("b".to_string()));
    assert_eq!(join_sk(None, None), None);
}

// ── display profiles: song vs speech (ROZHODNUTÉ on #217) ──────────────────

#[test]
fn dub_subtitles_are_speech_and_everything_else_is_a_song() {
    assert_eq!(
        DisplayProfile::for_source("gemini-live-translate"),
        DisplayProfile::Speech
    );
    assert_eq!(
        DisplayProfile::for_source("gemini-3-5-transcribe"),
        DisplayProfile::Song
    );
    assert_eq!(
        DisplayProfile::for_source("lrclib+mtl@rev1/g35t-ok"),
        DisplayProfile::Song
    );
    assert_eq!(DisplayProfile::for_source(""), DisplayProfile::Song);
}

#[test]
fn a_speech_line_shows_exactly_when_it_is_spoken() {
    // The same three lines under both profiles. Speech has no lead, but it
    // keeps the hold (through the 1000 ms gap) and the break tail (the
    // 9000 ms gap).
    let lines = [
        sk(1_000, 3_000, "Boh nás miluje viac, než si predstavíme."),
        sk(4_000, 6_000, "A volá nás k sebe každý deň."),
        sk(15_000, 17_000, "Preto mu dnes ďakujeme."),
    ];
    let speech = DisplayPlan::build(&lines, DisplayProfile::Speech);
    assert_eq!(speech.profile(), DisplayProfile::Speech);
    assert_eq!(
        spans(speech.lines()),
        [(1_000, 4_000), (4_000, 9_000), (15_000, 20_000)]
    );
    assert_eq!(index_at(&speech, 999), None);
    assert_eq!(index_at(&speech, 1_000), Some(0));
    assert_eq!(index_at(&speech, 3_999), Some(0));
    assert_eq!(index_at(&speech, 4_000), Some(1));

    // A song leads by 800 ms at the start and after the break. The second
    // line follows the first after only 1000 ms, so it gets no lead.
    let song = song_display_plan(&lines);
    assert_eq!(song.profile(), DisplayProfile::Song);
    assert_eq!(
        spans(song.lines()),
        [(200, 4_000), (4_000, 9_000), (14_200, 20_000)]
    );
}

#[test]
fn a_speech_line_is_never_held_back_for_the_one_before() {
    // Two dub sentences 300 ms apart. Speech shows each exactly when it is
    // spoken, even though the first is then up for only 300 ms. A song keeps
    // the first on the wall for 1200 ms.
    let lines = [sk(1_000, 1_300, "Áno."), sk(1_300, 1_600, "Amen.")];
    assert_eq!(
        spans(&build_plan(&lines, DisplayProfile::Speech)),
        [(1_000, 1_300), (1_300, 4_600)]
    );
    assert_eq!(
        spans(&build_plan(&lines, DisplayProfile::Song)),
        [(200, 1_400), (1_400, 4_600)]
    );
}

// ── show time: lead only into a pause, min visibility ──────────────────────

#[test]
fn a_line_shows_800_ms_before_it_is_sung() {
    let plan = song_plan(&[en(10_000, 12_000, "Seated on the throne of grace.")]);
    assert_eq!(spans(&plan), [(9_200, 15_000)]);
}

#[test]
fn the_first_lead_is_clamped_at_the_track_start() {
    let plan = song_plan(&[en(500, 2_500, "Seated on the throne of grace.")]);
    assert_eq!(spans(&plan), [(0, 5_500)]);
}

#[test]
fn a_line_leads_only_into_a_pause_after_the_previous_one() {
    // The previous sentence is sung until 12 000 ms. The next may appear
    // early only from 1500 ms after that (13 500), and at most 800 ms early.
    for (start, show) in [
        (12_000, 12_000),
        (13_500, 13_500),
        (14_000, 13_500),
        (14_300, 13_500),
        (15_000, 14_200),
    ] {
        let plan = song_plan(&[
            en(10_000, 12_000, "Seated on the throne of grace."),
            en(start, start + 2_000, "We give you the glory now."),
        ]);
        assert_eq!(plan[1].show_ms, show, "sung from {start}");
        assert_eq!(plan[0].hide_ms, show, "held until the next shows");
    }
}

#[test]
fn a_line_stays_1200_ms_on_the_wall_even_when_the_next_is_sung_sooner() {
    // "What a God, what a God." twice at 0.3 s each ("What A God" lines 29
    // and 30). The second waits until the first has been up for 1200 ms,
    // 100 ms after it starts being sung. The text is the same.
    let what_a_god = |start_ms: u64| {
        line(
            start_ms,
            start_ms + 300,
            "What a God, what a God.",
            "Aký Boh, aký Boh.",
        )
    };
    let plan = song_plan(&[what_a_god(97_000), what_a_god(97_300)]);
    assert_eq!(ranges(&plan), [0..1, 1..2]);
    assert_eq!(spans(&plan), [(96_200, 97_400), (97_400, 100_600)]);
}

#[test]
fn in_a_fast_run_of_short_sentences_each_keeps_1200_ms() {
    // Four sentences 1000 ms apart, each sung for 900 ms. The MIN_VISIBLE
    // floor comes after the cap at the sung start (design record
    // 5867952012), so the third and fourth show 200 and 400 ms after they
    // start being sung. Pinned so that a change of that order is deliberate.
    let lines: Vec<LyricsLine> = (0..4u64)
        .map(|k| {
            let start = 10_000 + 1_000 * k;
            en(start, start + 900, &format!("Line {k}."))
        })
        .collect();
    assert_eq!(
        spans(&song_plan(&lines)),
        [
            (9_200, 11_000),
            (11_000, 12_200),
            (12_200, 13_400),
            (13_400, 16_900)
        ]
    );
}

// ── hold / long break ──────────────────────────────────────────────────────

#[test]
fn a_line_holds_through_a_gap_until_the_next_line_shows() {
    let plan = song_plan(&[
        en(1_000, 3_000, "Seated on the throne of grace."),
        en(7_000, 9_000, "We give you the glory now."),
    ]);
    assert_eq!(spans(&plan), [(200, 6_200), (6_200, 12_000)]);
}

#[test]
fn a_gap_of_exactly_8000_ms_still_holds() {
    let plan = song_plan(&[
        en(10_000, 12_000, "Seated on the throne of grace."),
        en(20_000, 22_000, "We give you the glory now."),
    ]);
    assert_eq!(spans(&plan), [(9_200, 19_200), (19_200, 25_000)]);
}

#[test]
fn a_gap_over_8000_ms_hides_the_line_3_s_after_it_ends() {
    let plan = song_display_plan(&[
        en(10_000, 12_000, "Seated on the throne of grace."),
        en(20_001, 22_000, "We give you the glory now."),
    ]);
    assert_eq!(spans(plan.lines()), [(9_200, 15_000), (19_201, 25_000)]);
    assert_eq!(index_at(&plan, 9_199), None);
    assert_eq!(index_at(&plan, 9_200), Some(0));
    assert_eq!(index_at(&plan, 14_999), Some(0));
    assert_eq!(index_at(&plan, 15_000), None);
    assert_eq!(index_at(&plan, 19_200), None);
    assert_eq!(index_at(&plan, 19_201), Some(1));
    assert_eq!(index_at(&plan, 24_999), Some(1));
    assert_eq!(index_at(&plan, 25_000), None);
}

#[test]
fn at_finds_the_line_on_the_wall_with_half_open_bounds() {
    let plan = song_display_plan(&[
        en(1_000, 3_000, "Seated on the throne of grace."),
        en(7_000, 9_000, "We give you the glory now."),
    ]);
    assert_eq!(plan.lines().len(), 2);
    assert_eq!(index_at(&plan, 199), None);
    assert_eq!(index_at(&plan, 200), Some(0));
    assert_eq!(index_at(&plan, 6_199), Some(0));
    assert_eq!(index_at(&plan, 6_200), Some(1));
    assert_eq!(index_at(&plan, 11_999), Some(1));
    assert_eq!(index_at(&plan, 12_000), None);
    let (_, shown) = plan.at(6_200).expect("the second line is on the wall");
    assert_eq!(shown.en, "We give you the glory now.");
}

#[test]
fn an_empty_track_has_an_empty_plan() {
    let plan = song_display_plan(&[]);
    assert!(plan.lines().is_empty());
    assert_eq!(plan.at(0), None);
}
