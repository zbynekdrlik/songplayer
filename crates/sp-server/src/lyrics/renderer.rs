use sp_core::lyrics::LyricsTrack;
use sp_core::ws::ServerMsg;

use crate::lyrics::display_plan::{DisplayPlan, DisplayProfile};

/// DB key used to read the configured operator lead time. Operators can
/// override per-installation via `PATCH /api/v1/settings {"lyrics_lead_ms":
/// "500"}` if stage-display / LED-wall sync needs adjustment. When absent or
/// unparseable, it defaults to 0, meaning no EXTRA lead. The #217 display plan
/// already shows a wall line up to `display_plan::LEAD_MAX_MS` (0.8 s) before
/// it is sung, into a pause; this setting shifts the whole plan on top of that.
pub const LYRICS_LEAD_SETTING_KEY: &str = "lyrics_lead_ms";

/// Strip trailing punctuation (`,;:.!?…`) from a single display line. Stage
/// displays (Resolume, ProPresenter) by convention do not show end-of-line
/// punctuation — it doesn't add meaning when each line is one phrase, and
/// it visually clutters projected lyrics. Inner punctuation (mid-line
/// commas etc.) stays untouched. Whitespace is also trimmed at the end.
fn strip_display_punctuation(s: &str) -> String {
    // Trim trailing whitespace first so that lines like "Note: " (space after
    // colon) don't sneak the colon past the punctuation strip. Then strip
    // the punctuation, then trim once more in case there was whitespace
    // between the punctuation and a still-stripping run.
    s.trim_end()
        .trim_end_matches([',', ';', ':', '.', '!', '?', '…'])
        .trim_end()
        .to_string()
}

/// Append ` ★` (U+2605) to `s` when `is_reference` and `s` is non-empty
/// (#142). Called AFTER `strip_display_punctuation` so the star itself is
/// never stripped as trailing punctuation. An empty string stays empty —
/// a lone star on a blank display slot would be worse than no marker.
fn append_reference_star(s: String, is_reference: bool) -> String {
    if is_reference && !s.is_empty() {
        format!("{s} \u{2605}")
    } else {
        s
    }
}

/// #222: the lines of one Presenter push — the current and the next display
/// line in EN and SK, each pair from the same plan line (`presenter_lines`).
/// An SK is empty when its line has no translation; `next_*` are empty on the
/// last line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresenterLines {
    pub current_en: String,
    pub next_en: String,
    pub current_sk: String,
    pub next_sk: String,
}

/// A plan line's SK as the Presenter shows it: punctuation stripped like the
/// EN, "" when there is no line or no translation.
fn presenter_sk(line: Option<&crate::lyrics::display_plan::DisplayLine>) -> String {
    line.and_then(|l| l.sk.as_deref())
        .map(strip_display_punctuation)
        .unwrap_or_default()
}

/// Tracks playback position relative to a [`LyricsTrack`] and produces
/// [`ServerMsg::LyricsUpdate`] messages for the dashboard WebSocket.
pub struct LyricsState {
    track: LyricsTrack,
    /// What the LED wall and the Presenter show (#217): whole sentences,
    /// each held until the next line and shown per the track's
    /// `DisplayProfile`. A song leads by up to `LEAD_MAX_MS` only into a
    /// pause after the previous line; speech has no lead. Built once per
    /// loaded track. The dashboard path (`update`) keeps the raw `track` and
    /// its word timing.
    plan: DisplayPlan,
    /// Operator lead time (ms) shifted into every stage-display / LED-wall
    /// lookup, on top of the plan's own lead.
    /// `0` for the raw dashboard path (`update`) preserves the
    /// dashboard-highlighter-aligns-to-real-audio invariant.
    lead_ms: u64,
    /// Per-song time-axis shift in ms (from `videos.lyrics_time_offset_ms`).
    /// Applied at render time: every lookup searches
    /// `position_ms + lead_ms - offset_ms`. Positive delays the
    /// displayed line (shorter effective lead); negative advances it
    /// (longer effective lead). Arithmetic is saturating u64 — lookups
    /// clamp at 0 so large positive offsets early in playback don't
    /// underflow.
    offset_ms: i64,
}

/// Apply the (per-method-lead + offset) transform used by every render-side
/// lookup. Returns `position_ms + lead_ms - offset_ms`, saturating at 0 for
/// both positive-overflow and negative-underflow.
///
/// `lead_ms = self.lead_ms` for the stage-display / LED-wall paths
/// (`presenter_lines`, `resolume_lines_with_next`). `lead_ms = 0` for the
/// dashboard-highlighter path (`update`) per the
/// CLAUDE.md note that the dashboard must align to real playback.
#[inline]
fn effective_lookup(position_ms: u64, lead_ms: u64, offset_ms: i64) -> u64 {
    let after_lead = position_ms.saturating_add(lead_ms);
    if offset_ms >= 0 {
        after_lead.saturating_sub(offset_ms as u64)
    } else {
        after_lead.saturating_add(offset_ms.unsigned_abs())
    }
}

impl LyricsState {
    pub fn new(track: LyricsTrack) -> Self {
        Self::with_lead_and_offset(track, 0, 0)
    }

    /// Construct a state with an explicit lead AND per-song offset. The lead
    /// is read from the `lyrics_lead_ms` DB setting (0 by default); the offset
    /// is from the per-song `videos.lyrics_time_offset_ms` field. Builds the
    /// track's #217 display plan once, here, under the profile its `source`
    /// marks: a dub subtitle track is speech (no lead), anything else a song.
    pub fn with_lead_and_offset(track: LyricsTrack, lead_ms: u64, offset_ms: i64) -> Self {
        let profile = DisplayProfile::for_source(&track.source);
        let plan = DisplayPlan::build(&track.lines, profile);
        Self {
            track,
            plan,
            lead_ms,
            offset_ms,
        }
    }

    /// The display plan the wall and the Presenter read (#217).
    pub fn display_plan(&self) -> &DisplayPlan {
        &self.plan
    }

    /// Compute the [`ServerMsg::LyricsUpdate`] for the given playback position.
    ///
    /// Returns a message with all-`None` fields when the position falls between
    /// lines, so the dashboard can clear itself.
    pub fn update(&self, playlist_id: i64, position_ms: u64) -> ServerMsg {
        // Dashboard path: no lead so the karaoke highlighter aligns with the
        // actual audio position. Still honors `offset_ms` so operator shifts
        // are visible to the web dashboard, not just the stage display.
        let lookup = effective_lookup(position_ms, 0, self.offset_ms);
        let result = self.track.line_at(lookup);

        match result {
            None => ServerMsg::LyricsUpdate {
                playlist_id,
                line_en: None,
                line_sk: None,
                prev_line_en: None,
                next_line_en: None,
                active_word_index: None,
                word_count: None,
            },
            Some((idx, line)) => {
                let active_word_index = self.track.word_index_at(line, lookup);
                let word_count = line.words.as_ref().map(|w| w.len());

                let prev_line_en = if idx > 0 {
                    Some(self.track.lines[idx - 1].en.clone())
                } else {
                    None
                };

                let next_line_en = self.track.lines.get(idx + 1).map(|l| l.en.clone());

                ServerMsg::LyricsUpdate {
                    playlist_id,
                    line_en: Some(line.en.clone()),
                    line_sk: line.sk.clone(),
                    prev_line_en,
                    next_line_en,
                    active_word_index,
                    word_count,
                }
            }
        }
    }

    /// Returns `(current_en, next_en, current_sk, next_sk)` for the Resolume
    /// dual-line push, read from the #217 display plan. The current line is
    /// the plan's line on the wall at this position; `next_*` is the plan's
    /// NEXT display line. `next_en` is the empty string when the current line
    /// is the last one. `next_sk` is `None` when the current line is last or
    /// the next line has no SK translation. Returns `None` only before the
    /// first line, in the blank stretch of an instrumental break (a gap over
    /// `display_plan::LONG_GAP_MS`), and after the last line leaves. In every
    /// normal gap the line is held.
    ///
    /// The lookup is shifted forward by `self.lead_ms` (0 unless operator-overridden).
    ///
    /// `is_reference` (#142) — when true, every non-empty returned line gets
    /// ` ★` appended so the LED wall shows which songs carry Claude's
    /// verified reference lyrics. Applied AFTER `strip_display_punctuation`
    /// so the star is never stripped as trailing punctuation. An empty
    /// string (or `None`) stays empty/`None` — no lone star on a blank slot.
    pub fn resolume_lines_with_next(
        &self,
        position_ms: u64,
        is_reference: bool,
    ) -> Option<(String, String, Option<String>, Option<String>)> {
        let lookahead = effective_lookup(position_ms, self.lead_ms, self.offset_ms);
        let (idx, line) = self.plan.at(lookahead)?;
        let next_line = self.plan.lines().get(idx + 1);
        let cur_en = append_reference_star(strip_display_punctuation(&line.en), is_reference);
        let next_en = append_reference_star(
            next_line
                .map(|l| strip_display_punctuation(&l.en))
                .unwrap_or_default(),
            is_reference,
        );
        let cur_sk = line
            .sk
            .as_deref()
            .map(strip_display_punctuation)
            .map(|s| append_reference_star(s, is_reference));
        let next_sk = next_line
            .and_then(|l| l.sk.as_deref())
            .map(strip_display_punctuation)
            .map(|s| append_reference_star(s, is_reference));
        Some((cur_en, next_en, cur_sk, next_sk))
    }

    /// Returns the Presenter push's lines, read from the same #217 display
    /// plan as the wall: the current and the next display line, EN and (#222)
    /// SK, each pair from the SAME plan line, so the two languages never
    /// disagree about which line is current. `next_*` is empty on the last
    /// line, and an SK is empty when its line has no translation. Returns
    /// `None` where the wall is blank (before the first line, in an
    /// instrumental break's blank stretch, after the last line), so the
    /// caller can hold off pushing a duplicate.
    ///
    /// The lookup is shifted forward by `self.lead_ms` (0 unless operator-overridden).
    pub fn presenter_lines(&self, position_ms: u64) -> Option<PresenterLines> {
        let lookahead = effective_lookup(position_ms, self.lead_ms, self.offset_ms);
        let (idx, line) = self.plan.at(lookahead)?;
        let next = self.plan.lines().get(idx + 1);
        Some(PresenterLines {
            current_en: strip_display_punctuation(&line.en),
            next_en: next
                .map(|l| strip_display_punctuation(&l.en))
                .unwrap_or_default(),
            current_sk: presenter_sk(Some(line)),
            next_sk: presenter_sk(next),
        })
    }

    /// Read-only accessor for the underlying [`LyricsTrack`]. Used in tests
    /// and by callers that need metadata about lines without a position.
    pub fn track(&self) -> &sp_core::lyrics::LyricsTrack {
        &self.track
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::display_plan::DisplayProfile;
    use sp_core::lyrics::{LyricsLine, LyricsTrack, LyricsWord};

    #[test]
    fn strip_display_punctuation_removes_trailing_marks() {
        assert_eq!(strip_display_punctuation("Hello world,"), "Hello world");
        assert_eq!(strip_display_punctuation("Hello world."), "Hello world");
        assert_eq!(
            strip_display_punctuation("Praise the Lord!"),
            "Praise the Lord"
        );
        assert_eq!(strip_display_punctuation("Are you ready?"), "Are you ready");
        assert_eq!(strip_display_punctuation("Listen…"), "Listen");
        assert_eq!(strip_display_punctuation("End: line"), "End: line"); // colon mid-line stays
    }

    #[test]
    fn strip_display_punctuation_keeps_inner_punctuation() {
        // Mid-line commas / apostrophes must stay.
        assert_eq!(
            strip_display_punctuation("Won't you stay,"),
            "Won't you stay"
        );
        assert_eq!(
            strip_display_punctuation("Through every season, in spite of."),
            "Through every season, in spite of"
        );
    }

    #[test]
    fn strip_display_punctuation_handles_multiple_trailing() {
        assert_eq!(strip_display_punctuation("Wow!?"), "Wow");
        assert_eq!(strip_display_punctuation("End...."), "End");
        assert_eq!(strip_display_punctuation("Note: "), "Note");
    }

    #[test]
    fn strip_display_punctuation_preserves_text_without_punct() {
        assert_eq!(strip_display_punctuation("clean line"), "clean line");
        assert_eq!(strip_display_punctuation(""), "");
    }

    fn test_track() -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: "sk".into(),
            lines: vec![
                LyricsLine {
                    start_ms: 1000,
                    end_ms: 3000,
                    en: "Hello world".into(),
                    sk: Some("Ahoj svet".into()),
                    words: Some(vec![
                        LyricsWord {
                            text: "Hello".into(),
                            start_ms: 1000,
                            end_ms: 1800,
                        },
                        LyricsWord {
                            text: "world".into(),
                            start_ms: 2000,
                            end_ms: 3000,
                        },
                    ]),
                },
                LyricsLine {
                    start_ms: 3000,
                    end_ms: 5000,
                    en: "Goodbye".into(),
                    sk: Some("Zbohom".into()),
                    words: Some(vec![
                        LyricsWord {
                            text: "Good".into(),
                            start_ms: 3000,
                            end_ms: 3800,
                        },
                        LyricsWord {
                            text: "bye".into(),
                            start_ms: 4000,
                            end_ms: 5000,
                        },
                    ]),
                },
            ],
        }
    }

    /// `test_track()` as two sentences on wall timing (#217). The display
    /// plan groups lines into sentences, so the texts end in a period; the
    /// wall strips it. "Goodbye." is sung from 4000 ms, 1000 ms after "Hello
    /// world." ends, too soon for a lead into a pause. The plan shows them
    /// over [200, 4000) and [4000, 9000): the first leads by 800 ms.
    fn wall_track() -> LyricsTrack {
        let mut track = test_track();
        track.lines[0].en = "Hello world.".into();
        track.lines[1].en = "Goodbye.".into();
        track.lines[1].start_ms = 4_000;
        track.lines[1].end_ms = 6_000;
        track
    }

    #[test]
    fn update_emits_lyrics_for_active_line() {
        let state = LyricsState::new(test_track());
        let msg = state.update(1, 1500);
        match msg {
            ServerMsg::LyricsUpdate {
                playlist_id,
                line_en,
                line_sk,
                active_word_index,
                ..
            } => {
                assert_eq!(playlist_id, 1);
                assert_eq!(line_en, Some("Hello world".into()));
                assert_eq!(line_sk, Some("Ahoj svet".into()));
                // position 1500 is after word 0 start (1000), before word 1 start (2000)
                assert_eq!(active_word_index, Some(0));
            }
            _ => panic!("Expected LyricsUpdate"),
        }
    }

    #[test]
    fn update_emits_none_between_lines() {
        let state = LyricsState::new(test_track());
        // position 500 is before the first line (starts at 1000)
        let msg = state.update(1, 500);
        match msg {
            ServerMsg::LyricsUpdate {
                line_en,
                line_sk,
                prev_line_en,
                next_line_en,
                active_word_index,
                word_count,
                ..
            } => {
                assert_eq!(line_en, None);
                assert_eq!(line_sk, None);
                assert_eq!(prev_line_en, None);
                assert_eq!(next_line_en, None);
                assert_eq!(active_word_index, None);
                assert_eq!(word_count, None);
            }
            _ => panic!("Expected LyricsUpdate"),
        }
    }

    #[test]
    fn update_prev_next_lines() {
        let state = LyricsState::new(test_track());
        // position in second line
        let msg = state.update(1, 3500);
        match msg {
            ServerMsg::LyricsUpdate {
                line_en,
                prev_line_en,
                next_line_en,
                ..
            } => {
                assert_eq!(line_en, Some("Goodbye".into()));
                // prev is first line
                assert_eq!(prev_line_en, Some("Hello world".into()));
                // second line is the last, so next is None
                assert_eq!(next_line_en, None);
            }
            _ => panic!("Expected LyricsUpdate"),
        }
    }

    #[test]
    fn update_word_index_advances() {
        let state = LyricsState::new(test_track());
        // First update: position at start of first word
        let msg1 = state.update(1, 1000);
        let idx1 = match msg1 {
            ServerMsg::LyricsUpdate {
                active_word_index, ..
            } => active_word_index,
            _ => panic!("Expected LyricsUpdate"),
        };
        // Second update: position at start of second word
        let msg2 = state.update(1, 2000);
        let idx2 = match msg2 {
            ServerMsg::LyricsUpdate {
                active_word_index, ..
            } => active_word_index,
            _ => panic!("Expected LyricsUpdate"),
        };
        assert_eq!(idx1, Some(0));
        assert_eq!(idx2, Some(1));
        assert_ne!(idx1, idx2);
    }

    #[test]
    fn presenter_lines_returns_current_and_next() {
        let st = LyricsState::new(wall_track());
        // wall_track()'s plan shows line 0 over [200, 4000).
        let lines = st.presenter_lines(1500).expect("on line 0");
        assert_eq!(lines.current_en, "Hello world");
        // next_en is line 1's text.
        assert_eq!(lines.next_en, "Goodbye");
    }

    #[test]
    fn presenter_lines_returns_empty_next_for_last_line() {
        let st = LyricsState::new(wall_track());
        // Position 4500 is inside wall_track()'s last line (sung 4000..6000).
        // test_track()'s unpunctuated lines would be ONE sentence under #217.
        let nxt = st.presenter_lines(4500).expect("on last line").next_en;
        assert!(
            nxt.is_empty(),
            "last line's next must be empty, got {nxt:?}"
        );
    }

    /// The EN pair of a Presenter push (the plan-timing tests read only it).
    fn en_pair(lines: PresenterLines) -> (String, String) {
        (lines.current_en, lines.next_en)
    }

    // ── #222 — the Presenter gets the SK of the SAME plan line ────────────

    #[test]
    fn presenter_lines_carry_the_sk_of_the_same_plan_line() {
        let st = LyricsState::new(wall_track());
        assert_eq!(
            st.presenter_lines(1500),
            Some(PresenterLines {
                current_en: "Hello world".into(),
                next_en: "Goodbye".into(),
                current_sk: "Ahoj svet".into(),
                next_sk: "Zbohom".into(),
            })
        );
        // The last line: its own SK, and no next line in either language.
        assert_eq!(
            st.presenter_lines(4500),
            Some(PresenterLines {
                current_en: "Goodbye".into(),
                next_en: String::new(),
                current_sk: "Zbohom".into(),
                next_sk: String::new(),
            })
        );
    }

    #[test]
    fn a_presenter_line_without_a_translation_has_an_empty_sk() {
        let mut track = wall_track();
        // Trailing punctuation goes like the EN's; a line with no SK is "".
        track.lines[0].sk = Some("Ahoj svet!".into());
        track.lines[1].sk = None;
        let st = LyricsState::new(track);
        let lines = st.presenter_lines(1500).expect("on line 0");
        assert_eq!(
            (lines.current_sk.as_str(), lines.next_sk.as_str()),
            ("Ahoj svet", "")
        );
        let lines = st.presenter_lines(4500).expect("on line 1");
        assert_eq!(lines.current_sk, "");
    }

    #[test]
    fn presenter_lines_returns_none_before_first_line() {
        // With operator lead 0, the lookup is the position itself. The #217
        // plan shows the first line (sung at 5000 ms) 800 ms early, at 4200 ms,
        // so the wall is blank before that.
        let track = LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 5_000,
                end_ms: 7_000,
                en: "Later".into(),
                sk: None,
                words: None,
            }],
        };
        let st = LyricsState::new(track);
        assert!(st.presenter_lines(0).is_none());
        assert!(st.presenter_lines(4_199).is_none());
        assert_eq!(
            st.presenter_lines(4_200).map(en_pair),
            Some(("Later".to_string(), String::new()))
        );
    }

    #[test]
    fn resolume_lines_with_next_returns_all_four() {
        let st = LyricsState::new(wall_track());
        let (cur_en, next_en, cur_sk, _next_sk) =
            st.resolume_lines_with_next(1500, false).expect("on line 0");
        assert_eq!(cur_en, "Hello world");
        assert!(!next_en.is_empty(), "expected a next line text");
        assert!(cur_sk.is_some(), "current line has SK in test_track()");
    }

    // ── #142 — ★ reference marker on the Resolume dual-line push ──────────

    #[test]
    fn resolume_lines_with_next_appends_star_to_all_four_when_reference() {
        // is_reference=true: every non-empty EN/SK current+next line ends
        // with " ★" so the LED wall shows which songs carry Claude's
        // verified reference lyrics (#142).
        let st = LyricsState::new(wall_track());
        let (cur_en, next_en, cur_sk, next_sk) =
            st.resolume_lines_with_next(1500, true).expect("on line 0");
        assert_eq!(cur_en, "Hello world \u{2605}");
        assert_eq!(next_en, "Goodbye \u{2605}");
        assert_eq!(cur_sk, Some("Ahoj svet \u{2605}".to_string()));
        assert_eq!(next_sk, Some("Zbohom \u{2605}".to_string()));
    }

    #[test]
    fn resolume_lines_with_next_no_star_when_not_reference() {
        // is_reference=false: behavior is byte-identical to before #142.
        let st = LyricsState::new(wall_track());
        let (cur_en, next_en, cur_sk, next_sk) =
            st.resolume_lines_with_next(1500, false).expect("on line 0");
        assert_eq!(cur_en, "Hello world");
        assert_eq!(next_en, "Goodbye");
        assert_eq!(cur_sk, Some("Ahoj svet".to_string()));
        assert_eq!(next_sk, Some("Zbohom".to_string()));
    }

    #[test]
    fn resolume_lines_with_next_no_star_on_empty_lines_when_reference() {
        // Empty strings/None stay empty/None even when is_reference=true —
        // a lone " ★" on an empty next-line slot would be worse than no
        // marker at all.
        let st = LyricsState::new(wall_track());
        // Position 4500 is inside wall_track()'s last line (sung 4000..6000),
        // so no next line exists.
        let (cur_en, next_en, cur_sk, next_sk) = st
            .resolume_lines_with_next(4500, true)
            .expect("on last line");
        assert_eq!(
            cur_en, "Goodbye \u{2605}",
            "non-empty current line gets the star"
        );
        assert_eq!(next_en, "", "empty next_en must stay empty, no star");
        assert_eq!(cur_sk, Some("Zbohom \u{2605}".to_string()));
        assert_eq!(next_sk, None, "next_sk stays None, no star");
    }

    #[test]
    fn resolume_lines_with_next_returns_empty_next_on_last_line() {
        let st = LyricsState::new(wall_track());
        // Position 4500 is inside wall_track()'s last line (sung 4000..6000).
        // test_track()'s unpunctuated lines would be ONE sentence under #217.
        let (_cur, next_en, _cur_sk, next_sk) = st
            .resolume_lines_with_next(4500, false)
            .expect("on last line");
        assert!(next_en.is_empty(), "last-line next_en must be empty");
        assert!(next_sk.is_none(), "last-line next_sk must be None");
    }

    /// A single line sung at 3000 ms, with lead=0 and offset_ms = +500. The
    /// #217 display plan shows it 800 ms early, at 2200 ms. The positive
    /// offset delays it: the lookup is `position - 500`, so it first appears
    /// at playback 2700 ms.
    ///
    /// - Position 2699 → lookup 2199 → None.
    /// - Position 2700 → lookup 2200 → Some.
    ///
    /// Kills the `offset subtracted` mutant, which would show it from 1700 ms.
    #[test]
    fn applies_positive_offset_delays_line_start() {
        let track = LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 3_000,
                end_ms: 5_000,
                en: "Offset line".into(),
                sk: None,
                words: None,
            }],
        };
        let st = LyricsState::with_lead_and_offset(track, 0, 500);
        assert!(
            st.presenter_lines(2_699).is_none(),
            "positive offset must delay: lookup 2699-500=2199 is before the plan's show at 2200"
        );
        let cur = st
            .presenter_lines(2_700)
            .expect("lookup 2700-500=2200 is the plan's show time")
            .current_en;
        assert_eq!(cur, "Offset line");
    }

    /// A negative offset advances the displayed line: offset_ms = -1800 adds
    /// 1800 to every lookup. The #217 plan shows a line sung at 5000 ms from
    /// 4200 ms (800 ms early), so with the offset it appears at playback
    /// 2400 ms.
    ///
    /// - Position 2399 → lookup 4199 → None.
    /// - Position 2400 → lookup 4200 → Some.
    ///
    /// Kills a flipped offset sign, which would make the lookup 600 at 2400.
    #[test]
    fn applies_negative_offset_advances_line_start() {
        let track = LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 5_000,
                end_ms: 7_000,
                en: "Advanced line".into(),
                sk: None,
                words: None,
            }],
        };
        let st = LyricsState::with_lead_and_offset(track, 0, -1_800);
        assert!(
            st.presenter_lines(2_399).is_none(),
            "lookup 2399+1800=4199 is before the plan's show at 4200"
        );
        let cur = st
            .presenter_lines(2_400)
            .expect("negative offset must advance lookup onto the line")
            .current_en;
        assert_eq!(cur, "Advanced line");
    }

    /// Lead 0 and offset 0 leave the plan's own boundaries untouched, and
    /// `new` behaves exactly like `with_lead_and_offset(track, 0, 0)`.
    /// `wall_track()`'s plan is [200, 4000) "Hello world", then [4000, 9000)
    /// "Goodbye".
    #[test]
    fn offset_zero_behaves_identically_to_no_offset() {
        let st_new = LyricsState::new(wall_track());
        let st_off = LyricsState::with_lead_and_offset(wall_track(), 0, 0);
        for st in [&st_new, &st_off] {
            let cur = |pos| st.presenter_lines(pos).map(|l| l.current_en);
            assert_eq!(cur(199), None);
            assert_eq!(cur(200).as_deref(), Some("Hello world"));
            assert_eq!(cur(3_999).as_deref(), Some("Hello world"));
            assert_eq!(cur(4_000).as_deref(), Some("Goodbye"));
            assert_eq!(cur(8_999).as_deref(), Some("Goodbye"));
            assert_eq!(cur(9_000), None);
        }
        for pos in [0u64, 199, 200, 3_999, 4_000, 4_500, 9_000] {
            assert_eq!(
                st_new.resolume_lines_with_next(pos, false),
                st_off.resolume_lines_with_next(pos, false),
                "resolume_lines_with_next must match at position {pos}"
            );
        }
    }

    /// Constructor `with_lead_and_offset` parameterizes the operator's
    /// stage-display lead, which shifts every wall lookup ON TOP of the #217
    /// display plan. `wall_track()`'s plan switches from "Hello world" to
    /// "Goodbye" at 4000 ms, so with lead=500 the switch comes at playback
    /// 3500 ms.
    ///
    /// - Position 3499 + lead 500 = 3999 → still "Hello world".
    /// - Position 3500 + lead 500 = 4000 → "Goodbye".
    ///
    /// Together the two prove the lead is exactly 500 (not 0).
    #[test]
    fn lead_ms_is_applied_from_state() {
        let st = LyricsState::with_lead_and_offset(wall_track(), 500, 0);
        let cur = st
            .presenter_lines(3_500)
            .expect("3500 + lead(500) = 4000 = the plan's switch to Goodbye")
            .current_en;
        assert_eq!(cur, "Goodbye");
        let cur = st
            .presenter_lines(3_499)
            .expect("3499 + lead(500) = 3999: Hello world is still on the wall")
            .current_en;
        assert_eq!(cur, "Hello world");
    }

    // ── #217 — the wall and the Presenter read the display plan ────────────

    /// Two SK-translated sentences with the given sung ranges.
    fn two_line_track(first: (u64, u64), second: (u64, u64)) -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: "sk".into(),
            lines: vec![
                LyricsLine {
                    start_ms: first.0,
                    end_ms: first.1,
                    en: "Hello world.".into(),
                    sk: Some("Ahoj svet.".into()),
                    words: None,
                },
                LyricsLine {
                    start_ms: second.0,
                    end_ms: second.1,
                    en: "Goodbye.".into(),
                    sk: Some("Zbohom.".into()),
                    words: None,
                },
            ],
        }
    }

    /// A 4000 ms sung gap (3000..7000): the old lookup blanked the wall in
    /// it. Now "Hello world" is held until "Goodbye" shows, 800 ms before it
    /// is sung (6200).
    #[test]
    fn wall_holds_the_line_through_a_normal_gap() {
        let st = LyricsState::new(two_line_track((1_000, 3_000), (7_000, 9_000)));
        assert_eq!(
            st.resolume_lines_with_next(4_000, false),
            Some((
                "Hello world".to_string(),
                "Goodbye".to_string(),
                Some("Ahoj svet".to_string()),
                Some("Zbohom".to_string()),
            ))
        );
        assert_eq!(
            st.presenter_lines(6_199).map(en_pair),
            Some(("Hello world".to_string(), "Goodbye".to_string()))
        );
        assert_eq!(
            st.presenter_lines(6_200).map(en_pair),
            Some(("Goodbye".to_string(), String::new()))
        );
        assert_eq!(st.display_plan().lines().len(), 2);
    }

    /// An 8001 ms gap is an instrumental break. "Hello world" leaves 3000 ms
    /// after its end (6000), the wall is blank, and "Goodbye" shows 800 ms
    /// before it is sung (10 201).
    #[test]
    fn wall_blanks_only_in_a_long_break() {
        let st = LyricsState::new(two_line_track((1_000, 3_000), (11_001, 13_000)));
        assert!(st.resolume_lines_with_next(5_999, false).is_some());
        assert!(st.resolume_lines_with_next(6_000, false).is_none());
        assert!(st.presenter_lines(10_200).is_none());
        assert_eq!(
            st.presenter_lines(10_201).map(en_pair),
            Some(("Goodbye".to_string(), String::new()))
        );
    }

    /// The two 0.3 s halves of one sentence show as one wall line, and
    /// `next_*` is the plan's next DISPLAY line, not the next source line.
    /// "Angels bow before him" shows 800 ms before it is sung (4200).
    #[test]
    fn wall_shows_a_whole_sentence_and_the_plans_next_line() {
        let track = LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: "sk".into(),
            lines: vec![
                LyricsLine {
                    start_ms: 1_000,
                    end_ms: 1_300,
                    en: "What a God,".into(),
                    sk: Some("Aký Boh,".into()),
                    words: None,
                },
                LyricsLine {
                    start_ms: 1_300,
                    end_ms: 1_600,
                    en: "what a God.".into(),
                    sk: Some("aký Boh.".into()),
                    words: None,
                },
                LyricsLine {
                    start_ms: 5_000,
                    end_ms: 7_000,
                    en: "Angels bow before him".into(),
                    sk: Some("Anjeli sa mu klaňajú".into()),
                    words: None,
                },
            ],
        };
        let st = LyricsState::new(track);
        assert_eq!(
            st.resolume_lines_with_next(1_000, false),
            Some((
                "What a God, what a God".to_string(),
                "Angels bow before him".to_string(),
                Some("Aký Boh, aký Boh".to_string()),
                Some("Anjeli sa mu klaňajú".to_string()),
            ))
        );
        assert_eq!(
            st.presenter_lines(4_199).map(en_pair),
            Some((
                "What a God, what a God".to_string(),
                "Angels bow before him".to_string()
            ))
        );
        assert_eq!(
            st.presenter_lines(4_200).map(en_pair),
            Some(("Angels bow before him".to_string(), String::new()))
        );
    }

    /// A dub subtitle track (`source = "gemini-live-translate"`) is chosen as
    /// speech when it loads, so the wall shows each line exactly when it is
    /// spoken (no lead) and still holds it through the gap. A song track keeps
    /// the song profile.
    #[test]
    fn a_dub_track_loads_as_speech_and_shows_lines_exactly_when_spoken() {
        let mut track = two_line_track((1_000, 3_000), (4_000, 6_000));
        track.source = "gemini-live-translate".into();
        for line in &mut track.lines {
            line.en = String::new();
        }
        let st = LyricsState::new(track);
        assert_eq!(st.display_plan().profile(), DisplayProfile::Speech);
        assert_eq!(st.resolume_lines_with_next(999, false), None);
        assert_eq!(
            st.resolume_lines_with_next(1_000, false),
            Some((
                String::new(),
                String::new(),
                Some("Ahoj svet".to_string()),
                Some("Zbohom".to_string()),
            ))
        );
        let sk_at = |pos| st.resolume_lines_with_next(pos, false).and_then(|l| l.2);
        assert_eq!(sk_at(3_999).as_deref(), Some("Ahoj svet"));
        assert_eq!(sk_at(4_000).as_deref(), Some("Zbohom"));
        assert_eq!(
            LyricsState::new(wall_track()).display_plan().profile(),
            DisplayProfile::Song
        );
    }
}
