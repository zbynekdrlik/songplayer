//! Tests for the #217 LED-wall display plan on stored lyrics:
//! - "What A God" (`6KuPjo1diLg`), the song that blinked on the wall on
//!   2026-09-27;
//! - videos 335 and 221, the two sp-fast songs on which the owner saw the
//!   wall switch mid-sentence and light text long before it was sung
//!   (28.9.2026).
//!
//! All three are base tier (`gemini-3-5-transcribe`, v22), fetched from
//! win-resolume `/api/v1/videos/{id}/lyrics`. The pinned values were derived
//! with a line-by-line reference model of `build_plan`. The sentence and fit
//! checks use their own oracles below, written apart from `display_plan`.

use std::ops::Range;

use sp_core::lyrics::{LyricsLine, LyricsTrack};

use super::{
    DisplayLine, DisplayPlan, DisplayProfile, GROUP_MAX_SPAN_MS, HOLD_TAIL_MS, LEAD_MAX_MS,
    LONG_GAP_MS, MAX_CHARS, MIN_VISIBLE_MS, SUSTAIN_MARGIN_MS, build_plan,
};

fn parse(raw: &str) -> LyricsTrack {
    serde_json::from_str(raw).expect("the fixture parses as a LyricsTrack")
}

/// "What A God" by Indiana Bible College: 191 lines over 633 s.
fn what_a_god() -> LyricsTrack {
    parse(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/lyrics_6KuPjo1diLg.json"
    )))
}

/// Video 335 ("Lift up the gates, fling wide the doors…"): 105 lines.
fn video_335() -> LyricsTrack {
    parse(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/lyrics_335.json"
    )))
}

/// Video 221 ("A kind of explosion happened inside me…"): 75 lines.
fn video_221() -> LyricsTrack {
    parse(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/lyrics_221.json"
    )))
}

/// Every fixture with its name, for the rules that hold on all of them.
fn fixtures() -> [(&'static str, LyricsTrack); 3] {
    [
        ("What A God", what_a_god()),
        ("335", video_335()),
        ("221", video_221()),
    ]
}

/// The plan of sung lyrics (the Song profile).
fn song_plan(lines: &[LyricsLine]) -> Vec<DisplayLine> {
    build_plan(lines, DisplayProfile::Song)
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

/// Oracle: the text a line is grouped by, its EN or, with no EN, its SK.
fn text_of(line: &LyricsLine) -> &str {
    let en = line.en.trim();
    if en.is_empty() {
        line.sk.as_deref().unwrap_or_default().trim()
    } else {
        en
    }
}

/// Oracle: the line ends a sentence (`. ! ? …`, closing quotes and brackets
/// skipped).
fn ends_a_sentence(line: &LyricsLine) -> bool {
    text_of(line)
        .trim_end_matches(['"', '\'', '”', '’', '“', '»', '«', ')', ']'])
        .ends_with(['.', '!', '?', '…'])
}

/// Oracle: consecutive source lines fit one wall line. Their joined text is
/// at most `MAX_CHARS` chars, and the last starts at most
/// `GROUP_MAX_SPAN_MS` after the first.
fn fits_one_wall_line(members: &[LyricsLine]) -> bool {
    let texts: Vec<&str> = members
        .iter()
        .map(text_of)
        .filter(|t| !t.is_empty())
        .collect();
    let span = members[members.len() - 1].start_ms - members[0].start_ms;
    texts.join(" ").chars().count() <= MAX_CHARS && span <= GROUP_MAX_SPAN_MS
}

// ── the tracks ─────────────────────────────────────────────────────────────

#[test]
fn fixtures_are_the_stored_base_tier_tracks() {
    for ((name, track), count) in fixtures().into_iter().zip([191, 105, 75]) {
        assert_eq!(track.source, "gemini-3-5-transcribe", "{name}");
        assert_eq!(track.lines.len(), count, "{name}");
    }
    // Before #217 the wall blanked in every gap between the sung lines of
    // "What A God": 310.7 s.
    let sung_gaps: u64 = what_a_god()
        .lines
        .windows(2)
        .map(|w| w[1].start_ms.saturating_sub(w[0].end_ms))
        .sum();
    assert_eq!(sung_gaps, 310_700);
}

// ── whole sentences (design record 5867952012) ─────────────────────────────

#[test]
fn fixture_335_shows_whole_sentences() {
    let plan = song_plan(&video_335().lines);
    let by_src = |range: Range<usize>| {
        plan.iter()
            .find(|d| d.src_range == range)
            .unwrap_or_else(|| panic!("no display line for source lines {range:?}"))
    };
    assert_eq!(by_src(27..28).en, "And now his kingdom comes.");
    // "Now his will be" and "done." (sung 2.4 s later) are one sentence.
    assert_eq!(by_src(28..30).en, "Now his will be done.");
    // "Lift up your" / "banners and practice" / "your praise.", all three
    // times it is sung.
    for range in [9..12, 30..33, 88..91] {
        assert_eq!(
            by_src(range).en,
            "Lift up your banners and practice your praise."
        );
    }
}

#[test]
fn fixture_335_first_wall_lines() {
    // (source lines, show, hide) of the first 12 wall lines.
    let plan = song_plan(&video_335().lines);
    let first: Vec<(Range<usize>, u64, u64)> = plan
        .iter()
        .take(12)
        .map(|d| (d.src_range.clone(), d.show_ms, d.hide_ms))
        .collect();
    assert_eq!(
        first,
        [
            (0..3, 16_500, 24_200),
            (3..7, 24_200, 31_400),
            (7..8, 31_400, 34_988),
            (8..9, 34_988, 39_700),
            (9..12, 39_700, 44_708),
            (12..14, 44_708, 50_500),
            (14..15, 50_500, 54_300),
            (15..17, 54_300, 60_530),
            (17..19, 60_530, 65_682),
            (19..20, 65_682, 68_885),
            (20..21, 68_885, 73_000),
            (21..24, 85_100, 92_700),
        ]
    );
}

#[test]
fn fixture_display_lines_never_run_past_a_sentence_end() {
    for (name, track) in fixtures() {
        for d in song_plan(&track.lines) {
            let members = &track.lines[d.src_range.clone()];
            let inner_end = members[..members.len() - 1]
                .iter()
                .position(ends_a_sentence);
            assert_eq!(
                inner_end, None,
                "{name}: {:?} runs past a sentence end",
                d.en
            );
        }
    }
}

#[test]
fn fixture_a_sentence_splits_only_where_it_does_not_fit() {
    for (name, track) in fixtures() {
        let lines = &track.lines;
        let plan = song_plan(lines);
        for pair in plan.windows(2) {
            let (prev, d) = (&pair[0], &pair[1]);
            if ends_a_sentence(&lines[d.src_range.start - 1]) {
                continue;
            }
            // A split inside a sentence needs a break before `d`, or the
            // line that did not fit: the last of `d` when `d` ends its
            // sentence, else the line after `d`.
            let gap = sung_start(lines, d).saturating_sub(sung_end(lines, prev));
            let last = d.src_range.end - 1;
            let through = if ends_a_sentence(&lines[last]) {
                last
            } else {
                (last + 1).min(lines.len() - 1)
            };
            assert!(
                gap > LONG_GAP_MS || !fits_one_wall_line(&lines[prev.src_range.start..=through]),
                "{name}: {:?} | {:?} splits a sentence that fits",
                prev.en,
                d.en
            );
        }
    }
}

#[test]
fn fixture_every_wall_line_fits() {
    for (name, track) in fixtures() {
        for d in song_plan(&track.lines) {
            let members = &track.lines[d.src_range.clone()];
            assert!(
                members.len() == 1 || fits_one_wall_line(members),
                "{name}: {:?} is over {MAX_CHARS} chars or {GROUP_MAX_SPAN_MS} ms",
                d.en
            );
        }
    }
}

// ── show time: lead only into a pause ──────────────────────────────────────

#[test]
fn fixture_lead_is_at_most_800_ms_and_only_into_a_pause() {
    for (name, track) in fixtures() {
        let lines = &track.lines;
        let plan = song_plan(lines);
        for (i, d) in plan.iter().enumerate() {
            let start = sung_start(lines, d);
            let lead = start.saturating_sub(d.show_ms);
            assert!(lead <= LEAD_MAX_MS, "{name}: {:?} leads {lead} ms", d.en);
            let Some(prev) = i.checked_sub(1).map(|p| &plan[p]) else {
                continue;
            };
            if lead > 0 {
                assert!(
                    d.show_ms >= sung_end(lines, prev) + SUSTAIN_MARGIN_MS,
                    "{name}: {:?} leads while {:?} was sung under 1.5 s ago",
                    d.en,
                    prev.en
                );
            }
            if d.show_ms > start {
                assert_eq!(
                    d.show_ms,
                    prev.show_ms + MIN_VISIBLE_MS,
                    "{name}: only the MIN_VISIBLE floor may hold {:?} back",
                    d.en
                );
            }
        }
    }
}

#[test]
fn fixture_lead_counts() {
    // (full 800 ms leads, exact shows, late shows) per fixture. The one
    // late show is "What A God"'s second 0.3 s "What a God, what a God.",
    // 100 ms after it starts being sung.
    for ((name, track), counts) in
        fixtures()
            .into_iter()
            .zip([(41, 65, 1), (13, 44, 0), (9, 38, 0)])
    {
        let lines = &track.lines;
        let plan = song_plan(lines);
        let full = plan
            .iter()
            .filter(|d| d.show_ms + LEAD_MAX_MS == sung_start(lines, d))
            .count();
        let exact = plan
            .iter()
            .filter(|d| d.show_ms == sung_start(lines, d))
            .count();
        let late = plan
            .iter()
            .filter(|d| d.show_ms > sung_start(lines, d))
            .count();
        assert_eq!((full, exact, late), counts, "{name}");
    }
}

// ── visibility / hold / long break ─────────────────────────────────────────

#[test]
fn fixture_no_wall_line_is_up_for_less_than_1200_ms() {
    // (wall lines, the shortest time one is up) per fixture.
    for ((name, track), (count, shortest)) in
        fixtures()
            .into_iter()
            .zip([(123, 1_200), (59, 1_685), (51, 1_321)])
    {
        let plan = song_plan(&track.lines);
        assert_eq!(plan.len(), count, "{name}");
        let up = plan
            .iter()
            .map(|d| d.hide_ms.saturating_sub(d.show_ms))
            .min()
            .expect("the plan is not empty");
        assert!(up >= MIN_VISIBLE_MS, "{name}: a line is up for {up} ms");
        assert_eq!(up, shortest, "{name}");
    }
}

#[test]
fn fixture_no_wall_line_leaves_before_it_is_sung_to_the_end() {
    for (name, track) in fixtures() {
        let lines = &track.lines;
        for d in song_plan(lines) {
            let end = sung_end(lines, &d);
            assert!(
                d.hide_ms >= end,
                "{name}: {:?} leaves at {} ms but is sung until {end} ms",
                d.en,
                d.hide_ms
            );
        }
    }
}

#[test]
fn fixture_the_wall_blanks_only_in_instrumental_breaks() {
    // (breaks over 8 s, total blank ms between the first and last line).
    for ((name, track), expected) in
        fixtures()
            .into_iter()
            .zip([(3, 19_600), (2, 28_400), (4, 31_700)])
    {
        let lines = &track.lines;
        let plan = song_plan(lines);
        let mut breaks = 0;
        let mut blank_ms = 0;
        for pair in plan.windows(2) {
            let (cur, next) = (&pair[0], &pair[1]);
            let gap = sung_start(lines, next).saturating_sub(sung_end(lines, cur));
            if gap <= LONG_GAP_MS {
                assert_eq!(
                    cur.hide_ms, next.show_ms,
                    "{name}: blank before {:?}",
                    next.en
                );
            } else {
                breaks += 1;
                assert_eq!(cur.hide_ms, sung_end(lines, cur) + HOLD_TAIL_MS, "{name}");
                assert_eq!(
                    next.show_ms + LEAD_MAX_MS,
                    sung_start(lines, next),
                    "{name}"
                );
                blank_ms += next.show_ms - cur.hide_ms;
            }
        }
        assert_eq!((breaks, blank_ms), expected, "{name}");
    }
}

#[test]
fn fixture_what_a_god_long_breaks_hide_3_s_after_the_line_ends() {
    let plan = DisplayPlan::build(&what_a_god().lines, DisplayProfile::Song);
    // [start of the break's blank, next line's show) for the three breaks.
    for (blank_from, next_show, last_text) in [
        (205_800, 211_600, "To you are"),
        (432_200, 440_000, "No."),
        (547_200, 553_200, "What a God, what a God."),
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
    assert_eq!(index_at(&plan, 999), None);
    assert_eq!(index_at(&plan, 1_000), Some(0));
    let last = plan.lines().len() - 1;
    assert_eq!(index_at(&plan, 635_999), Some(last));
    assert_eq!(index_at(&plan, 636_000), None);
}

// ── text ───────────────────────────────────────────────────────────────────

#[test]
fn fixture_source_lines_tile_the_plan_in_order() {
    for (name, track) in fixtures() {
        let mut next = 0;
        for d in song_plan(&track.lines) {
            assert_eq!(d.src_range.start, next, "{name}");
            assert!(d.src_range.end > next, "{name}");
            next = d.src_range.end;
        }
        assert_eq!(next, track.lines.len(), "{name}");
    }
}

#[test]
fn fixture_en_and_sk_stay_paired() {
    for (name, track) in fixtures() {
        for d in song_plan(&track.lines) {
            let src = &track.lines[d.src_range.clone()];
            let en: Vec<&str> = src.iter().map(|l| l.en.trim()).collect();
            let sk: Vec<&str> = src
                .iter()
                .map(|l| l.sk.as_deref().unwrap_or_default().trim())
                .collect();
            assert_eq!(d.en, en.join(" "), "{name}");
            assert_eq!(d.sk.as_deref(), Some(sk.join(" ").as_str()), "{name}");
        }
    }
}

#[test]
fn fixture_what_a_god_sentences() {
    let lines = what_a_god().lines;
    let plan = song_plan(&lines);
    let by_src = |range: Range<usize>| {
        plan.iter()
            .find(|d| d.src_range == range)
            .unwrap_or_else(|| panic!("no display line for source lines {range:?}"))
    };
    // The doubled 0.3 s "What a God, what a God." is two sentences, the
    // second held back 100 ms so the first is up for 1200 ms.
    let first = by_src(29..30);
    let second = by_src(30..31);
    assert_eq!(first.en, "What a God, what a God.");
    assert_eq!(second.en, "What a God, what a God.");
    assert_eq!((first.show_ms, first.hide_ms), (96_200, 97_400));
    assert_eq!((second.show_ms, second.hide_ms), (97_400, 99_200));
    // Sentences split over three source lines join whole.
    assert_eq!(
        by_src(128..131).en,
        "I searched all over and I still couldn't find nobody no."
    );
    assert_eq!(
        by_src(6..9).en,
        "You're nothing like I thought you were you're better."
    );
}
