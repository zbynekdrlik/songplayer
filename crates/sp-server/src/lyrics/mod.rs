pub mod aligner;
pub mod assembly;
pub mod autosub_provider;
pub mod bootstrap;
pub mod chunking;
pub mod description_provider;
pub mod lrclib;
pub mod merge;
pub mod orchestrator;
pub mod provider;
pub mod quality;
pub mod qwen3_provider;
pub mod renderer;
pub mod reprocess;
pub mod text_merge;
pub mod translator;
pub mod worker;
pub mod youtube_subs;
pub use worker::LyricsWorker;
pub use worker::queue_update_loop;

use sp_core::lyrics::LyricsTrack;

/// Monotonic version of the lyrics pipeline output. Bump when prompts, provider
/// list, merge algorithm, or reference-text selection changes. Every bump
/// triggers auto-reprocess of existing songs via the stale-version bucket.
///
/// Version history:
/// - v1 (implicit, pre-this-PR): single-path yt_subs→Qwen3 or lrclib-line-level
/// - v2 (this PR): ensemble orchestrator with AutoSubProvider + Claude text-merge
/// - v3 (this PR): merge prompt reworked — weight by base_confidence^2,
///   prefer higher-confidence provider on >1000ms disagreement. Fixes
///   regression seen on h-A1Tzkjsi4 (v2 got 0.48 vs baseline 0.63).
pub const LYRICS_PIPELINE_VERSION: u32 = 3;

/// Clean a lyrics track by removing noise from auto-generated subtitles.
///
/// - Strips inline bracketed noise like `[music]`, `[applause]`, `[laughter]`
/// - Removes `>>` speaker turn markers
/// - Drops lines that are empty or consist only of noise after cleanup
pub fn clean_lyrics_track(track: &mut LyricsTrack) {
    for line in &mut track.lines {
        line.en = clean_subtitle_text(&line.en);
    }
    track.lines.retain(|line| !line.en.is_empty());
}

fn clean_subtitle_text(text: &str) -> String {
    let mut result = text.to_string();
    // Remove all bracketed content: [music], [applause], [laughter], etc.
    while let Some(open) = result.find('[') {
        if let Some(close) = result[open..].find(']') {
            result.replace_range(open..open + close + 1, "");
        } else {
            break;
        }
    }
    // Remove >> speaker markers
    result = result.replace(">>", "");
    // Collapse multiple spaces and trim
    let result: String = result.split_whitespace().collect::<Vec<_>>().join(" ");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_standalone_music() {
        assert_eq!(clean_subtitle_text("[music]"), "");
        assert_eq!(clean_subtitle_text("[Music]"), "");
        assert_eq!(clean_subtitle_text("[applause]"), "");
    }

    #[test]
    fn clean_inline_music() {
        assert_eq!(
            clean_subtitle_text("Jesus, we're [music] undone by you"),
            "Jesus, we're undone by you"
        );
    }

    #[test]
    fn clean_multiple_brackets() {
        assert_eq!(
            clean_subtitle_text("[music] Hello [applause] world [music]"),
            "Hello world"
        );
    }

    #[test]
    fn clean_speaker_markers() {
        assert_eq!(
            clean_subtitle_text(">> And I won't stand by"),
            "And I won't stand by"
        );
    }

    #[test]
    fn clean_combined() {
        assert_eq!(clean_subtitle_text(">> forever [music]"), "forever");
    }

    #[test]
    fn clean_leaves_normal_text() {
        assert_eq!(
            clean_subtitle_text("Amazing grace how sweet the sound"),
            "Amazing grace how sweet the sound"
        );
    }

    #[test]
    fn clean_empty_after_strip() {
        assert_eq!(clean_subtitle_text("[music]  [applause]"), "");
    }

    #[test]
    fn clean_track_removes_empty_lines() {
        let mut track = LyricsTrack {
            version: 1,
            source: "youtube".to_string(),
            language_source: "en".to_string(),
            language_translation: String::new(),
            lines: vec![
                sp_core::lyrics::LyricsLine {
                    start_ms: 0,
                    end_ms: 1000,
                    en: "[music]".to_string(),
                    sk: None,
                    words: None,
                },
                sp_core::lyrics::LyricsLine {
                    start_ms: 1000,
                    end_ms: 2000,
                    en: "Real lyrics here".to_string(),
                    sk: None,
                    words: None,
                },
                sp_core::lyrics::LyricsLine {
                    start_ms: 2000,
                    end_ms: 3000,
                    en: "[applause]".to_string(),
                    sk: None,
                    words: None,
                },
            ],
        };
        clean_lyrics_track(&mut track);
        assert_eq!(track.lines.len(), 1);
        assert_eq!(track.lines[0].en, "Real lyrics here");
    }
}
