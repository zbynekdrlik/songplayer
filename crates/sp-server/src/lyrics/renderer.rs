use sp_core::lyrics::LyricsTrack;
use sp_core::ws::ServerMsg;

/// DB key used to read the configured lead time. Operators can override
/// per-installation via `PATCH /api/v1/settings {"lyrics_lead_ms": "500"}`
/// if stage-display / LED-wall sync needs adjustment. When absent or
/// unparseable, defaults to 0 (no lead — line timing is real, per WhisperX
/// alignment accuracy).
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

/// Tracks playback position relative to a [`LyricsTrack`] and produces
/// [`ServerMsg::LyricsUpdate`] messages for the dashboard WebSocket.
pub struct LyricsState {
    track: LyricsTrack,
    /// Lead time (ms) shifted into every stage-display / LED-wall lookup.
    /// `0` for raw paths (`update`, `resolume_lines`) preserves the
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
/// dashboard-highlighter paths (`update`, `resolume_lines`) per the
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
        Self {
            track,
            lead_ms: 0,
            offset_ms: 0,
        }
    }

    /// Construct a state with an explicit lead AND per-song offset. The lead
    /// is read from the `lyrics_lead_ms` DB setting (0 by default); the offset
    /// is from the per-song `videos.lyrics_time_offset_ms` field.
    pub fn with_lead_and_offset(track: LyricsTrack, lead_ms: u64, offset_ms: i64) -> Self {
        Self {
            track,
            lead_ms,
            offset_ms,
        }
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

    /// Returns `(en_text, sk_text)` for the line active at `position_ms`.
    /// Returns `(None, None)` when between lines.
    ///
    /// Dashboard/raw path: no lead, but the per-song `offset_ms` is applied
    /// so operator shifts affect the wall playthrough consistently.
    pub fn resolume_lines(&self, position_ms: u64) -> (Option<String>, Option<String>) {
        let lookup = effective_lookup(position_ms, 0, self.offset_ms);
        match self.track.line_at(lookup) {
            None => (None, None),
            Some((_, line)) => (Some(line.en.clone()), line.sk.clone()),
        }
    }

    /// Returns `(current_en, next_en, current_sk, next_sk)` for the Resolume
    /// dual-line push. `next_en` is the empty string when the current line is
    /// the last line of the track. `next_sk` is `None` when the current line
    /// is last or when the next line has no SK translation.
    ///
    /// The lookup is shifted forward by `self.lead_ms` (0 unless operator-overridden).
    pub fn resolume_lines_with_next(
        &self,
        position_ms: u64,
    ) -> Option<(String, String, Option<String>, Option<String>)> {
        let lookahead = effective_lookup(position_ms, self.lead_ms, self.offset_ms);
        let (idx, line) = self.track.line_at(lookahead)?;
        let next_line = self.track.lines.get(idx + 1);
        let cur_en = strip_display_punctuation(&line.en);
        let next_en = next_line
            .map(|l| strip_display_punctuation(&l.en))
            .unwrap_or_default();
        let cur_sk = line.sk.as_deref().map(strip_display_punctuation);
        let next_sk = next_line
            .and_then(|l| l.sk.as_deref())
            .map(strip_display_punctuation);
        Some((cur_en, next_en, cur_sk, next_sk))
    }

    /// Returns `Some((current_en, next_en))` for the Presenter push when
    /// playback position is on a line. `next_en` is the empty string when
    /// the current line is the last line of the track. Returns `None`
    /// between lines so the caller can hold off pushing a duplicate.
    ///
    /// The lookup is shifted forward by `self.lead_ms` (0 unless operator-overridden).
    pub fn presenter_lines(&self, position_ms: u64) -> Option<(String, String)> {
        let lookahead = effective_lookup(position_ms, self.lead_ms, self.offset_ms);
        let (idx, line) = self.track.line_at(lookahead)?;
        let cur = strip_display_punctuation(&line.en);
        let next = self
            .track
            .lines
            .get(idx + 1)
            .map(|l| strip_display_punctuation(&l.en))
            .unwrap_or_default();
        Some((cur, next))
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
    fn resolume_lines_returns_text() {
        let state = LyricsState::new(test_track());
        // position inside first line
        let (en, sk) = state.resolume_lines(1500);
        assert_eq!(en, Some("Hello world".into()));
        assert_eq!(sk, Some("Ahoj svet".into()));
    }

    #[test]
    fn resolume_lines_returns_none_between_lines() {
        let state = LyricsState::new(test_track());
        // position before any line
        let (en, sk) = state.resolume_lines(500);
        assert_eq!(en, None);
        assert_eq!(sk, None);
    }

    #[test]
    fn presenter_lines_returns_current_and_next() {
        let st = LyricsState::new(test_track());
        // First line starts at 1000 ms per `test_track()`.
        let (cur, nxt) = st.presenter_lines(1500).expect("on line 0");
        assert_eq!(cur, "Hello world");
        // next_en should be line 1's text.
        assert!(!nxt.is_empty(), "expected a next line");
    }

    #[test]
    fn presenter_lines_returns_empty_next_for_last_line() {
        let st = LyricsState::new(test_track());
        // New default is 0 lead, so position 3200 looks up at exactly 3200 ms,
        // which is inside the last line (3000..5000).
        let (_cur, nxt) = st.presenter_lines(3200).expect("on last line");
        assert!(
            nxt.is_empty(),
            "last line's next must be empty, got {nxt:?}"
        );
    }

    #[test]
    fn presenter_lines_returns_none_before_first_line() {
        // With 0 lead (the new default), lookup at position 0 is exactly 0 ms,
        // which is before the first line (starts at 5000 ms).
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
        // position 0 + lead 0 = 0 ms, before 5000 ms.
        assert!(st.presenter_lines(0).is_none());
    }

    #[test]
    fn resolume_lines_with_next_returns_all_four() {
        let st = LyricsState::new(test_track());
        let (cur_en, next_en, cur_sk, _next_sk) =
            st.resolume_lines_with_next(1500).expect("on line 0");
        assert_eq!(cur_en, "Hello world");
        assert!(!next_en.is_empty(), "expected a next line text");
        assert!(cur_sk.is_some(), "current line has SK in test_track()");
    }

    #[test]
    fn resolume_lines_with_next_returns_empty_next_on_last_line() {
        let st = LyricsState::new(test_track());
        // With 0 lead, position 3200 looks up at exactly 3200 ms, which is
        // inside the last line (3000..5000).
        let (_cur, next_en, _cur_sk, next_sk) =
            st.resolume_lines_with_next(3200).expect("on last line");
        assert!(next_en.is_empty(), "last-line next_en must be empty");
        assert!(next_sk.is_none(), "last-line next_sk must be None");
    }

    /// Track with a single line starting at 1000 ms, lead=0, offset_ms = +500.
    /// At playback position 0 ms: effective lookup = 0 + lead(0) - offset(500) = 0
    /// (saturating subtraction) — still before the 1000 ms line start, so
    /// `presenter_lines` must return None. Kills the `offset subtracted` mutant.
    #[test]
    fn applies_positive_offset_delays_line_start() {
        let track = LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 1_000,
                end_ms: 3_000,
                en: "Offset line".into(),
                sk: None,
                words: None,
            }],
        };
        let st = LyricsState::with_lead_and_offset(track, 0, 500);
        assert!(
            st.presenter_lines(0).is_none(),
            "positive offset must delay — lookup 0+0-500=0 (saturating) is before line start 1000"
        );
    }

    /// Negative offset advances the displayed line: offset_ms = -500
    /// effectively adds to the lead. At position 200 ms with lead=0:
    /// effective lookup = 200 + lead(0) - (-500) = 700 ms — before the line
    /// (starts at 2000 ms), so None. With offset -1500: 200 + 0 - (-1500) = 1700
    /// — still before 2000 → None. Need offset -1800: 200 + 0 - (-1800) = 2000
    /// — exactly the line start → Some.
    #[test]
    fn applies_negative_offset_advances_line_start() {
        let track = LyricsTrack {
            version: 1,
            source: "test".into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines: vec![LyricsLine {
                start_ms: 2_000,
                end_ms: 4_000,
                en: "Advanced line".into(),
                sk: None,
                words: None,
            }],
        };
        // With lead=0 and offset -1800: 200 + 0 - (-1800) = 2000 = line start.
        let st = LyricsState::with_lead_and_offset(track, 0, -1_800);
        let (cur, _nxt) = st
            .presenter_lines(200)
            .expect("negative offset must advance lookup onto the line");
        assert_eq!(cur, "Advanced line");
    }

    /// With offset 0 and default lead (0 in new codebase), `LyricsState::with_lead_and_offset`
    /// must behave identically to `LyricsState::new`. Kills any mutant that
    /// folds the offset branch into a different path when offset == 0.
    #[test]
    fn offset_zero_behaves_identically_to_no_offset() {
        let st_new = LyricsState::new(test_track());
        let st_off = LyricsState::with_lead_and_offset(test_track(), 0, 0);
        for pos in [0u64, 500, 1500, 3200, 4500] {
            assert_eq!(
                st_new.presenter_lines(pos),
                st_off.presenter_lines(pos),
                "presenter_lines must match at position {pos}"
            );
            assert_eq!(
                st_new.resolume_lines(pos),
                st_off.resolume_lines(pos),
                "resolume_lines must match at position {pos}"
            );
            assert_eq!(
                st_new.resolume_lines_with_next(pos),
                st_off.resolume_lines_with_next(pos),
                "resolume_lines_with_next must match at position {pos}"
            );
        }
    }

    /// Constructor `with_lead_and_offset` parameterizes the stage-display
    /// lead. With lead=500 and no offset, position 500 + lead 500 = 1000
    /// which is exactly line 0's start — `presenter_lines(500)` must
    /// return the line. With lead=0 (the current default), position 500
    /// would look at exactly 500 ms (before line 0 start at 1000). This test
    /// demonstrates that the lead parameter is actually applied.
    ///
    /// Position 499 + lead 500 = 999 < 1000 → None (just below line 0 start).
    /// Position 500 + lead 500 = 1000 = line 0 start → Some.
    /// So line 0 being Some at pos 500 AND None at pos 499 proves the
    /// lead is 500 (not 0).
    #[test]
    fn lead_ms_is_applied_from_state() {
        let st = LyricsState::with_lead_and_offset(test_track(), 500, 0);
        let (cur, _nxt) = st
            .presenter_lines(500)
            .expect("500 + lead(500) = 1000 = line 0 start, expected a line");
        assert_eq!(cur, "Hello world");
        // With lead=500, position 499 must NOT reach line 0 at 1000.
        assert!(
            st.presenter_lines(499).is_none(),
            "lead=500: pos 499 + 500 = 999 < 1000 line-start, must be None"
        );
    }
}
