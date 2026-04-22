use sp_core::lyrics::LyricsTrack;
use sp_core::ws::ServerMsg;

/// Tracks playback position relative to a [`LyricsTrack`] and produces
/// [`ServerMsg::LyricsUpdate`] messages for the dashboard WebSocket.
pub struct LyricsState {
    track: LyricsTrack,
}

impl LyricsState {
    pub fn new(track: LyricsTrack) -> Self {
        Self { track }
    }

    /// Compute the [`ServerMsg::LyricsUpdate`] for the given playback position.
    ///
    /// Returns a message with all-`None` fields when the position falls between
    /// lines, so the dashboard can clear itself.
    pub fn update(&self, playlist_id: i64, position_ms: u64) -> ServerMsg {
        let result = self.track.line_at(position_ms);

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
                let active_word_index = self.track.word_index_at(line, position_ms);
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
    pub fn resolume_lines(&self, position_ms: u64) -> (Option<String>, Option<String>) {
        match self.track.line_at(position_ms) {
            None => (None, None),
            Some((_, line)) => (Some(line.en.clone()), line.sk.clone()),
        }
    }

    /// Returns `(current_en, next_en, current_sk, next_sk)` for the Resolume
    /// dual-line push. `next_en` is the empty string when the current line is
    /// the last line of the track. `next_sk` is `None` when the current line
    /// is last or when the next line has no SK translation.
    pub fn resolume_lines_with_next(
        &self,
        position_ms: u64,
    ) -> Option<(String, String, Option<String>, Option<String>)> {
        let (idx, line) = self.track.line_at(position_ms)?;
        let next_line = self.track.lines.get(idx + 1);
        let next_en = next_line.map(|l| l.en.clone()).unwrap_or_default();
        let next_sk = next_line.and_then(|l| l.sk.clone());
        Some((line.en.clone(), next_en, line.sk.clone(), next_sk))
    }

    /// Returns `Some((current_en, next_en))` for the Presenter push when
    /// playback position is on a line. `next_en` is the empty string when
    /// the current line is the last line of the track. Returns `None`
    /// between lines so the caller can hold off pushing a duplicate.
    pub fn presenter_lines(&self, position_ms: u64) -> Option<(String, String)> {
        let (idx, line) = self.track.line_at(position_ms)?;
        let next = self
            .track
            .lines
            .get(idx + 1)
            .map(|l| l.en.clone())
            .unwrap_or_default();
        Some((line.en.clone(), next))
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
        // Jump to a timestamp guaranteed to be on the LAST line of
        // test_track(); empty next indicates end-of-song for the push.
        let last_line = st.track().lines.last().expect("track non-empty");
        let mid = (last_line.start_ms + last_line.end_ms) / 2;
        let (_cur, nxt) = st.presenter_lines(mid).expect("on last line");
        assert!(
            nxt.is_empty(),
            "last line's next must be empty, got {nxt:?}"
        );
    }

    #[test]
    fn presenter_lines_returns_none_between_lines() {
        let st = LyricsState::new(test_track());
        // position 0 is before first line (which starts at 1000 ms)
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
        let last = test_track().lines.last().cloned().expect("non-empty");
        let mid = (last.start_ms + last.end_ms) / 2;
        let (_cur, next_en, _cur_sk, next_sk) =
            st.resolume_lines_with_next(mid).expect("on last line");
        assert!(next_en.is_empty(), "last-line next_en must be empty");
        assert!(next_sk.is_none(), "last-line next_sk must be None");
    }
}
