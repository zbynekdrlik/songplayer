//! Tests for the #217 LED-wall display plan. They run on the real "What A
//! God" (`6KuPjo1diLg`) lyrics, the song that blinked on the wall during the
//! 2026-09-27 event, plus synthetic edge cases with exact boundaries. The
//! expected values were derived by hand and cross-checked against a line-by-line
//! reference model of `build_plan`.

use std::ops::Range;

use sp_core::lyrics::{LyricsLine, LyricsTrack};

use super::{
    DisplayLine, DisplayPlan, FRAGMENT_MAX_MS, FRAGMENT_MAX_WORDS, Group, HOLD_TAIL_MS, LEAD_MS,
    LONG_GAP_MS, MERGE_MAX_CHARS, MERGE_MAX_GAP_MS, MIN_VISIBLE_MS, build_plan, fits, is_fragment,
    join_sk, join_text, starts_lowercase,
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

fn group(start_ms: u64, end_ms: u64, en: &str) -> Group {
    Group::of(0, &line(start_ms, end_ms, en, ""))
}

fn texts(plan: &[DisplayLine]) -> Vec<&str> {
    plan.iter().map(|d| d.en.as_str()).collect()
}

/// "What A God" by Indiana Bible College, the base tier (`gemini-3-5-transcribe`,
/// v22): 191 lines over 633 s.
fn fixture() -> LyricsTrack {
    let raw = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/lyrics_6KuPjo1diLg.json"
    ));
    serde_json::from_str(raw).expect("the fixture parses as a LyricsTrack")
}

/// When the display line's first source line starts being sung.
fn sung_start(lines: &[LyricsLine], d: &DisplayLine) -> u64 {
    lines[d.src_range.start].start_ms
}

/// When the display line's last word is sung.
fn sung_end(lines: &[LyricsLine], d: &DisplayLine) -> u64 {
    lines[d.src_range.clone()]
        .iter()
        .map(|l| l.end_ms)
        .max()
        .expect("a display line has at least one source line")
}

fn index_at(plan: &DisplayPlan, position_ms: u64) -> Option<usize> {
    plan.at(position_ms).map(|(i, _)| i)
}

// ── constants ──────────────────────────────────────────────────────────────

#[test]
fn constants_match_the_design_record() {
    assert_eq!(LEAD_MS, 1_500);
    assert_eq!(MIN_VISIBLE_MS, 1_200);
    assert_eq!(LONG_GAP_MS, 8_000);
    assert_eq!(HOLD_TAIL_MS, 3_000);
    assert_eq!(FRAGMENT_MAX_MS, 1_500);
    assert_eq!(FRAGMENT_MAX_WORDS, 3);
    assert_eq!(MERGE_MAX_GAP_MS, 700);
    assert_eq!(MERGE_MAX_CHARS, 64);
}

// ── fragment / merge predicates ────────────────────────────────────────────

#[test]
fn a_line_sung_under_1500_ms_is_a_fragment_and_1500_ms_is_not() {
    let four_words = "Seated on the throne";
    assert!(is_fragment(&group(0, 1_499, four_words), None));
    assert!(!is_fragment(&group(0, 1_500, four_words), None));
}

#[test]
fn three_words_are_a_fragment_and_four_are_not() {
    assert!(is_fragment(&group(0, 2_000, "Seated on the"), None));
    assert!(!is_fragment(&group(0, 2_000, "Seated on the throne"), None));
    assert!(is_fragment(&group(0, 2_000, "  Seated   on  the  "), None));
}

#[test]
fn a_line_before_a_lowercase_continuation_is_a_fragment() {
    let whole = group(0, 2_000, "Seated on the throne");
    let continuation = group(2_000, 4_000, "and we give you glory");
    let new_sentence = group(2_000, 4_000, "And we give you glory");
    assert!(is_fragment(&whole, Some(&continuation)));
    assert!(!is_fragment(&whole, Some(&new_sentence)));
}

#[test]
fn starts_lowercase_reads_the_first_letter_or_digit() {
    assert!(starts_lowercase("and then"));
    assert!(starts_lowercase("'cause I"));
    assert!(starts_lowercase("…and then"));
    assert!(!starts_lowercase("And then"));
    assert!(!starts_lowercase("10,000 reasons"));
    assert!(!starts_lowercase("..."));
    assert!(!starts_lowercase(""));
}

#[test]
fn fits_allows_a_gap_of_at_most_700_ms() {
    let a = group(0, 1_000, "What a God,");
    assert!(fits(&a, &group(1_700, 2_000, "what a God.")));
    assert!(!fits(&a, &group(1_701, 2_000, "what a God.")));
    // Overlapping lines have no gap.
    assert!(fits(&a, &group(900, 2_000, "what a God.")));
}

#[test]
fn fits_allows_a_joined_text_of_at_most_64_chars() {
    // 31 + 1 space + 32 = 64 chars; one more char is 65.
    let a = group(0, 1_000, &"a".repeat(31));
    assert!(fits(&a, &group(1_000, 2_000, &"b".repeat(32))));
    assert!(!fits(&a, &group(1_000, 2_000, &"b".repeat(33))));
    // Chars, not bytes: 64 two-byte chars still fit.
    let sk = group(0, 1_000, &"č".repeat(31));
    assert!(fits(&sk, &group(1_000, 2_000, &"ž".repeat(32))));
    // "Oh" next to a 62-char line: 2 + 1 + 62 = 65 chars do not fit.
    let tiny = group(0, 1_000, "Oh");
    assert!(!fits(&tiny, &group(1_000, 2_000, &"b".repeat(62))));
}

#[test]
fn fits_allows_a_joined_slovak_text_of_at_most_64_chars() {
    // The EN fits ("Oh God"); only the SK decides: 31 + 1 + 32 = 64 fits,
    // 31 + 1 + 33 = 65 does not.
    let a = Group::of(0, &line(0, 1_000, "Oh", &"x".repeat(31)));
    let sk_64 = Group::of(1, &line(1_000, 2_000, "God", &"y".repeat(32)));
    let sk_65 = Group::of(1, &line(1_000, 2_000, "God", &"y".repeat(33)));
    assert!(fits(&a, &sk_64));
    assert!(!fits(&a, &sk_65));
    // A missing SK side counts as empty.
    let no_sk = Group {
        sk: None,
        ..Group::of(1, &line(1_000, 2_000, "God", ""))
    };
    assert!(fits(&a, &no_sk));
}

#[test]
fn a_dub_subtitle_track_never_collapses_into_one_giant_sk_line() {
    // Dub subtitle tracks (`gemini-live-translate`, #182/#184) load into the
    // same LyricsState as songs. Their lines touch, often carry no EN, and
    // hold a whole SK sentence. An empty EN always "fits", so only an SK
    // limit keeps them from merging into one huge Slovak block.
    let sk = "Boh nás miluje viac, než si dokážeme predstaviť, a volá nás k sebe.";
    let lines: Vec<LyricsLine> = (0..10u64)
        .map(|k| line(1_000 + 4_000 * k, 5_000 + 4_000 * k, "", sk))
        .collect();
    let plan = build_plan(&lines);
    assert_eq!(plan.len(), 10);
    assert!(plan.iter().all(|d| d.sk.as_deref() == Some(sk)));
    // Two short dub lines with no EN still merge, because their SK fits.
    let short = build_plan(&[
        line(1_000, 1_500, "", "Áno."),
        line(1_500, 2_000, "", "Amen."),
    ]);
    assert_eq!(short.len(), 1);
    assert_eq!(short[0].sk.as_deref(), Some("Áno. Amen."));
    assert_eq!(short[0].en, "");
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

// ── merging (synthetic) ────────────────────────────────────────────────────

#[test]
fn two_short_repeats_merge_into_one_wall_line() {
    // "What a God, what a God." twice at 0.3 s each (fixture lines 29 + 30).
    let plan = build_plan(&[
        line(
            97_000,
            97_300,
            "What a God, what a God.",
            "Aký Boh, aký Boh.",
        ),
        line(
            97_300,
            97_600,
            "What a God, what a God.",
            "Aký Boh, aký Boh.",
        ),
    ]);
    assert_eq!(
        plan,
        [DisplayLine {
            en: "What a God, what a God. What a God, what a God.".into(),
            sk: Some("Aký Boh, aký Boh. Aký Boh, aký Boh.".into()),
            show_ms: 95_500,
            hide_ms: 100_600,
            src_range: 0..2,
        }]
    );
}

#[test]
fn a_fragment_chain_merges_greedily_forward() {
    let plan = build_plan(&[
        line(1_000, 1_300, "What a God,", "Aký Boh,"),
        line(1_300, 1_600, "what a", "aký"),
        line(1_600, 1_900, "God.", "Boh."),
        line(
            5_000,
            7_000,
            "Angels bow before him now",
            "Anjeli sa mu klaňajú",
        ),
    ]);
    assert_eq!(
        texts(&plan),
        ["What a God, what a God.", "Angels bow before him now"]
    );
    assert_eq!(plan[0].sk.as_deref(), Some("Aký Boh, aký Boh."));
    assert_eq!(plan[0].src_range, 0..3);
    assert_eq!(plan[1].src_range, 3..4);
}

#[test]
fn a_fragment_that_cannot_merge_forward_merges_backward() {
    // "Oh God" is 701 ms before the next line (too far), so it joins the
    // line before it.
    let plan = build_plan(&[
        line(
            0,
            2_500,
            "Seated on the throne of grace",
            "Sedíš na tróne milosti",
        ),
        line(2_500, 3_000, "Oh God", "Ó Bože"),
        line(
            3_701,
            6_000,
            "We give you the glory now",
            "Vzdávame ti slávu teraz",
        ),
    ]);
    assert_eq!(
        texts(&plan),
        [
            "Seated on the throne of grace Oh God",
            "We give you the glory now"
        ]
    );
    assert_eq!(plan[0].sk.as_deref(), Some("Sedíš na tróne milosti Ó Bože"));
    assert_eq!(plan[0].src_range, 0..2);
    assert_eq!((plan[1].show_ms, plan[1].hide_ms), (2_201, 9_000));
}

#[test]
fn a_fragment_that_fits_both_neighbours_joins_the_line_it_leads_into() {
    let plan = build_plan(&[
        line(0, 2_500, "Seated on the throne of grace", "a"),
        line(2_500, 3_000, "Oh God", "b"),
        line(3_000, 5_000, "We give you the glory now", "c"),
    ]);
    assert_eq!(
        texts(&plan),
        [
            "Seated on the throne of grace",
            "Oh God We give you the glory now"
        ]
    );
    assert_eq!(plan[1].sk.as_deref(), Some("b c"));
}

#[test]
fn a_fragment_stays_alone_when_no_neighbour_is_within_700_ms() {
    let plan = build_plan(&[
        line(0, 2_000, "Seated on the throne of grace", ""),
        line(2_701, 3_000, "Oh", "Ó"),
        line(3_701, 6_000, "We give you the glory now", ""),
    ]);
    assert_eq!(
        texts(&plan),
        [
            "Seated on the throne of grace",
            "Oh",
            "We give you the glory now"
        ]
    );
    // "Oh" still stays on the wall for the full 1200 ms.
    assert_eq!((plan[1].show_ms, plan[1].hide_ms), (1_201, 2_401));
}

#[test]
fn a_merge_never_grows_a_wall_line_past_64_chars() {
    let a = "a".repeat(31);
    let fits_64 = build_plan(&[line(0, 500, &a, ""), line(500, 1_000, &"b".repeat(32), "")]);
    assert_eq!(fits_64.len(), 1);
    let over_64 = build_plan(&[line(0, 500, &a, ""), line(500, 1_000, &"b".repeat(33), "")]);
    assert_eq!(over_64.len(), 2);
}

#[test]
fn a_whole_line_joins_its_lowercase_continuation() {
    // The first line is long and wordy, but the next one starts lowercase
    // mid-sentence, so the first is a fragment and takes it. The third line
    // starts a new sentence and stays alone. Checking the lowercase rule on a
    // line's OWN text would instead pair the continuation with the third.
    let plan = build_plan(&[
        line(0, 2_000, "Seated on the throne of grace", "a"),
        line(2_000, 4_000, "and we give you the glory", "b"),
        line(4_000, 6_000, "Holy is the Lord our God", "c"),
    ]);
    assert_eq!(
        texts(&plan),
        [
            "Seated on the throne of grace and we give you the glory",
            "Holy is the Lord our God"
        ]
    );
    assert_eq!(plan[0].sk.as_deref(), Some("a b"));
}

#[test]
fn whole_lines_that_would_fit_together_stay_separate() {
    let plan = build_plan(&[
        line(0, 2_000, "There is no one higher", ""),
        line(2_000, 4_000, "There is no one greater", ""),
    ]);
    assert_eq!(
        texts(&plan),
        ["There is no one higher", "There is no one greater"]
    );
}

#[test]
fn a_line_left_under_1200_ms_on_the_wall_merges_when_it_fits() {
    // Two whole (non-fragment) lines that overlap: the second starts 1199 ms
    // after the first shows, so the first would be up for only 1199 ms.
    let plan = build_plan(&[
        line(1_000, 4_000, "Seated on the throne of grace", "a"),
        line(1_199, 4_500, "We give you the glory now", "b"),
    ]);
    assert_eq!(
        plan,
        [DisplayLine {
            en: "Seated on the throne of grace We give you the glory now".into(),
            sk: Some("a b".into()),
            show_ms: 0,
            hide_ms: 7_500,
            src_range: 0..2,
        }]
    );
}

#[test]
fn a_line_left_exactly_1200_ms_on_the_wall_stays_alone() {
    let plan = build_plan(&[
        line(1_000, 4_000, "Seated on the throne of grace", "a"),
        line(1_200, 4_500, "We give you the glory now", "b"),
    ]);
    assert_eq!(plan.len(), 2);
    assert_eq!((plan[0].show_ms, plan[0].hide_ms), (0, 1_200));
    assert_eq!((plan[1].show_ms, plan[1].hide_ms), (1_200, 7_500));
}

// ── lead / hold / long break (synthetic) ───────────────────────────────────

#[test]
fn a_line_shows_1500_ms_before_it_is_sung() {
    let plan = build_plan(&[line(10_000, 12_000, "Seated on the throne of grace", "")]);
    assert_eq!((plan[0].show_ms, plan[0].hide_ms), (8_500, 15_000));
}

#[test]
fn the_first_lead_is_clamped_at_the_track_start() {
    let plan = build_plan(&[line(1_000, 3_000, "Seated on the throne of grace", "")]);
    assert_eq!((plan[0].show_ms, plan[0].hide_ms), (0, 6_000));
}

#[test]
fn a_line_holds_through_a_gap_until_the_next_line_shows() {
    let plan = build_plan(&[
        line(1_000, 3_000, "Seated on the throne of grace", ""),
        line(7_000, 9_000, "We give you the glory now", ""),
    ]);
    assert_eq!((plan[0].show_ms, plan[0].hide_ms), (0, 5_500));
    assert_eq!((plan[1].show_ms, plan[1].hide_ms), (5_500, 12_000));
}

#[test]
fn a_gap_of_exactly_8000_ms_still_holds() {
    let plan = build_plan(&[
        line(10_000, 12_000, "Seated on the throne of grace", ""),
        line(20_000, 22_000, "We give you the glory now", ""),
    ]);
    assert_eq!((plan[0].show_ms, plan[0].hide_ms), (8_500, 18_500));
    assert_eq!((plan[1].show_ms, plan[1].hide_ms), (18_500, 25_000));
}

#[test]
fn a_gap_over_8000_ms_hides_the_line_3_s_after_it_ends() {
    let plan = DisplayPlan::build(&[
        line(10_000, 12_000, "Seated on the throne of grace", ""),
        line(20_001, 22_000, "We give you the glory now", ""),
    ]);
    let lines = plan.lines();
    assert_eq!((lines[0].show_ms, lines[0].hide_ms), (8_500, 15_000));
    assert_eq!((lines[1].show_ms, lines[1].hide_ms), (18_501, 25_000));
    assert_eq!(index_at(&plan, 8_499), None);
    assert_eq!(index_at(&plan, 8_500), Some(0));
    assert_eq!(index_at(&plan, 14_999), Some(0));
    assert_eq!(index_at(&plan, 15_000), None);
    assert_eq!(index_at(&plan, 18_500), None);
    assert_eq!(index_at(&plan, 18_501), Some(1));
    assert_eq!(index_at(&plan, 24_999), Some(1));
    assert_eq!(index_at(&plan, 25_000), None);
}

#[test]
fn a_fast_passage_switches_exactly_on_time_when_no_lead_is_left() {
    // Nine lines 1000 ms apart, each too long to merge with its neighbour
    // (37 + 1 + 37 > 64 chars). Each line keeps 1200 ms on the wall, which
    // eats the next line's lead until the ninth shows exactly when sung.
    // Under the design as written, lines 0 and 1 leave before they are sung
    // (9700 < 10000, 10900 < 11000); see the design question on #217.
    let lines: Vec<LyricsLine> = (0..9u64)
        .map(|k| {
            let start = 10_000 + 1_000 * k;
            line(start, start + 900, &format!("{k} {}", "x".repeat(35)), "")
        })
        .collect();
    let plan = build_plan(&lines);
    let shows: Vec<u64> = plan.iter().map(|d| d.show_ms).collect();
    assert_eq!(
        shows,
        [
            8_500, 9_700, 10_900, 12_100, 13_300, 14_500, 15_700, 16_900, 18_000
        ]
    );
    for pair in plan.windows(2) {
        assert_eq!(pair[0].hide_ms, pair[1].show_ms);
    }
    assert_eq!(plan[8].hide_ms, 21_900);
    // Only the eighth line gets less than 1200 ms. That is allowed, because
    // the source lines start only 1000 ms apart.
    assert_eq!(plan[7].hide_ms - plan[7].show_ms, 1_100);
}

#[test]
fn at_finds_the_line_on_the_wall_with_half_open_bounds() {
    let plan = DisplayPlan::build(&[
        line(1_000, 3_000, "Seated on the throne of grace", ""),
        line(7_000, 9_000, "We give you the glory now", ""),
    ]);
    assert_eq!(plan.lines().len(), 2);
    assert_eq!(index_at(&plan, 0), Some(0));
    assert_eq!(index_at(&plan, 5_499), Some(0));
    assert_eq!(index_at(&plan, 5_500), Some(1));
    assert_eq!(index_at(&plan, 11_999), Some(1));
    assert_eq!(index_at(&plan, 12_000), None);
    let (_, shown) = plan.at(5_500).expect("the second line is on the wall");
    assert_eq!(shown.en, "We give you the glory now");
}

#[test]
fn an_empty_track_has_an_empty_plan() {
    let plan = DisplayPlan::build(&[]);
    assert!(plan.lines().is_empty());
    assert_eq!(plan.at(0), None);
}

// ── the "What A God" fixture ───────────────────────────────────────────────

#[test]
fn fixture_is_the_what_a_god_base_tier_track() {
    let track = fixture();
    assert_eq!(track.source, "gemini-3-5-transcribe");
    assert_eq!(track.lines.len(), 191);
    // Before #217 the wall blanked in every gap between sung lines: 310.7 s.
    let sung_gaps: u64 = track
        .lines
        .windows(2)
        .map(|w| w[1].start_ms.saturating_sub(w[0].end_ms))
        .sum();
    assert_eq!(sung_gaps, 310_700);
}

#[test]
fn fixture_wall_never_blanks_in_a_gap_of_8_s_or_less() {
    let lines = fixture().lines;
    let plan = build_plan(&lines);
    let mut blank_ms = 0;
    let mut long_breaks = 0;
    for pair in plan.windows(2) {
        let (cur, next) = (&pair[0], &pair[1]);
        let gap = sung_start(&lines, next).saturating_sub(sung_end(&lines, cur));
        if gap <= LONG_GAP_MS {
            assert_eq!(cur.hide_ms, next.show_ms, "a blank before {:?}", next.en);
        } else {
            long_breaks += 1;
            assert_eq!(cur.hide_ms, sung_end(&lines, cur) + HOLD_TAIL_MS);
            assert_eq!(next.show_ms, sung_start(&lines, next) - LEAD_MS);
            blank_ms += next.show_ms - cur.hide_ms;
        }
    }
    // Only the three instrumental breaks (9.6 s, 11.6 s, 9.8 s) blank the wall:
    // 5.1 s + 7.1 s + 5.3 s, down from 310.7 s.
    assert_eq!(long_breaks, 3);
    assert_eq!(blank_ms, 17_500);
}

#[test]
fn fixture_no_wall_line_is_up_for_less_than_1200_ms() {
    let lines = fixture().lines;
    let plan = build_plan(&lines);
    for pair in plan.windows(2) {
        let up = pair[0].hide_ms - pair[0].show_ms;
        if up < MIN_VISIBLE_MS {
            let apart = sung_start(&lines, &pair[1]) - sung_start(&lines, &pair[0]);
            assert!(
                apart < MIN_VISIBLE_MS,
                "{:?} blinks for {up} ms",
                pair[0].en
            );
        }
    }
    let shortest = plan
        .iter()
        .map(|d| d.hide_ms - d.show_ms)
        .min()
        .expect("the plan is not empty");
    // The shortest is "All I have", on the wall for 508.1–509.5 s. Note that
    // it is sung only at 509.6 s: the next line's lead replaces it first (the
    // design question on #217). The old wall flashed 42 lines for under 1 s.
    assert_eq!(shortest, 1_400);
}

#[test]
fn fixture_lead_bounds_hold() {
    let lines = fixture().lines;
    let plan = build_plan(&lines);
    for (i, d) in plan.iter().enumerate() {
        let start = sung_start(&lines, d);
        assert!(d.show_ms <= start, "{:?} shows after it is sung", d.en);
        assert!(d.show_ms + LEAD_MS >= start, "{:?} shows too early", d.en);
        if i > 0 {
            let prev = &plan[i - 1];
            assert!(
                d.show_ms >= prev.show_ms + MIN_VISIBLE_MS || d.show_ms == start,
                "{:?} cuts {:?} short",
                d.en,
                prev.en
            );
        }
    }
    // This song leaves room everywhere: every line gets the full 1.5 s lead.
    assert!(
        plan.iter()
            .all(|d| sung_start(&lines, d) - d.show_ms == LEAD_MS)
    );
    assert_eq!(plan[0].show_ms, 300);
}

#[test]
fn fixture_merges_fragments_into_verses() {
    let lines = fixture().lines;
    let plan = build_plan(&lines);
    assert_eq!(plan.len(), 137);
    let by_src = |range: Range<usize>| {
        plan.iter()
            .find(|d| d.src_range == range)
            .unwrap_or_else(|| panic!("no display line for source lines {range:?}"))
    };
    // "What a God, what a God." twice at 0.3 s.
    assert_eq!(
        by_src(29..31).en,
        "What a God, what a God. What a God, what a God."
    );
    // "find nobody no" joins its sentence.
    assert_eq!(
        by_src(128..131).en,
        "I searched all over and I still couldn't find nobody no."
    );
    // A verse split in three mid-sentence.
    assert_eq!(
        by_src(6..9).en,
        "You're nothing like I thought you were you're better."
    );
    // A lone "Oh," with no neighbour within 700 ms stays alone, but it gets
    // 2.2 s instead of flashing for 0.5 s.
    let oh = by_src(11..12);
    assert_eq!(
        (oh.en.as_str(), oh.show_ms, oh.hide_ms),
        ("Oh,", 32_500, 34_700)
    );
    // Every merged line fits the wall, in both languages.
    assert!(
        plan.iter()
            .filter(|d| d.src_range.len() > 1)
            .all(|d| d.en.chars().count() <= MERGE_MAX_CHARS
                && d.sk.as_deref().unwrap_or_default().chars().count() <= MERGE_MAX_CHARS)
    );
    // The source lines tile the plan in order, none lost or repeated.
    let mut next = 0;
    for d in &plan {
        assert_eq!(d.src_range.start, next);
        assert!(d.src_range.end > next);
        next = d.src_range.end;
    }
    assert_eq!(next, lines.len());
}

#[test]
fn fixture_en_and_sk_stay_paired() {
    let lines = fixture().lines;
    let plan = build_plan(&lines);
    for d in &plan {
        let src = &lines[d.src_range.clone()];
        let en: Vec<&str> = src.iter().map(|l| l.en.trim()).collect();
        let sk: Vec<&str> = src
            .iter()
            .map(|l| l.sk.as_deref().unwrap_or_default().trim())
            .collect();
        assert_eq!(d.en, en.join(" "));
        assert_eq!(d.sk.as_deref(), Some(sk.join(" ").as_str()));
    }
}

#[test]
fn fixture_long_breaks_hide_3_s_after_the_line_ends() {
    let plan = DisplayPlan::build(&fixture().lines);
    // [start of the break's blank, next line's show) for the three breaks.
    for (blank_from, next_show, last_text) in [
        (205_800, 210_900, "To you are"),
        (432_200, 439_300, "No."),
        (547_200, 552_500, "What a God, what a God."),
    ] {
        let (i, held) = plan
            .at(blank_from - 1)
            .expect("the line is held until 3 s after it ends");
        assert_eq!(held.en, last_text);
        assert_eq!(held.hide_ms, blank_from);
        assert_eq!(index_at(&plan, blank_from), None);
        assert_eq!(index_at(&plan, next_show - 1), None);
        assert_eq!(index_at(&plan, next_show), Some(i + 1));
    }
    // Before the first line and after the last one, the wall is blank.
    assert_eq!(index_at(&plan, 299), None);
    assert_eq!(index_at(&plan, 300), Some(0));
    let last = plan.lines().len() - 1;
    assert_eq!(index_at(&plan, 635_999), Some(last));
    assert_eq!(index_at(&plan, 636_000), None);
}
