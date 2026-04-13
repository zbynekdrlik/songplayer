use sp_core::lyrics::LyricsTrack;
use sp_core::ws::ServerMsg;

/// Tracks playback position relative to a [`LyricsTrack`] and produces
/// [`ServerMsg::LyricsUpdate`] messages for the dashboard WebSocket.
pub struct LyricsState {
    track: LyricsTrack,
    last_line_index: Option<usize>,
}

impl LyricsState {
    pub fn new(track: LyricsTrack) -> Self {
        Self {
            track,
            last_line_index: None,
        }
    }

    /// Compute the [`ServerMsg::LyricsUpdate`] for the given playback position.
    ///
    /// Always returns `Some` — callers receive a message with all-`None` fields
    /// when the position falls between lines, so the dashboard can clear itself.
    pub fn update(&mut self, playlist_id: i64, position_ms: u64) -> Option<ServerMsg> {
        let result = self.track.line_at(position_ms);

        let msg = match result {
            None => {
                self.last_line_index = None;
                ServerMsg::LyricsUpdate {
                    playlist_id,
                    line_en: None,
                    line_sk: None,
                    prev_line_en: None,
                    next_line_en: None,
                    active_word_index: None,
                    word_count: None,
                }
            }
            Some((idx, line)) => {
                self.last_line_index = Some(idx);

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
        };

        Some(msg)
    }

    /// Returns `(en_text, sk_text)` for the line active at `position_ms`.
    /// Returns `(None, None)` when between lines.
    pub fn resolume_lines(&self, position_ms: u64) -> (Option<String>, Option<String>) {
        match self.track.line_at(position_ms) {
            None => (None, None),
            Some((_, line)) => (Some(line.en.clone()), line.sk.clone()),
        }
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
        let mut state = LyricsState::new(test_track());
        let msg = state.update(1, 1500).unwrap();
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
        let mut state = LyricsState::new(test_track());
        // position 500 is before the first line (starts at 1000)
        let msg = state.update(1, 500).unwrap();
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
        let mut state = LyricsState::new(test_track());
        // position in second line
        let msg = state.update(1, 3500).unwrap();
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
        let mut state = LyricsState::new(test_track());
        // First update: position at start of first word
        let msg1 = state.update(1, 1000).unwrap();
        let idx1 = match msg1 {
            ServerMsg::LyricsUpdate {
                active_word_index, ..
            } => active_word_index,
            _ => panic!("Expected LyricsUpdate"),
        };
        // Second update: position at start of second word
        let msg2 = state.update(1, 2000).unwrap();
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
}
