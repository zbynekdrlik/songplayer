pub mod aligner;
pub mod assembly;
pub mod autosub_provider;
pub mod bootstrap;
pub mod chunking;
pub mod description_provider;
pub mod gemini_chunks;
pub mod gemini_client;
pub mod gemini_parse;
pub mod gemini_prompt;
pub mod gemini_provider;
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
/// - v4: description provider added as 4th text candidate (YouTube video
///   description parsed via Claude). Targets recovering from v3 regression
///   (0.524 -> >= 0.65) by giving text_merge reliable reference text on
///   songs lacking yt_subs/lrclib coverage.
/// - v5: description provider prompt reframed to software-engineering task
///   (empty system, karaoke-app framing in user) — Claude on CLIProxyAPI OAuth
///   was returning conversational preamble instead of JSON under the previous
///   direct-instruction prompt, yielding 0% extraction success on production.
/// - v6: merge-layer fallback — when Claude miscounts per-word timings (1-6
///   off vs reference split_whitespace), fall back to the highest-
///   base-confidence provider's timings instead of dropping the song. Root
///   cause: tokenization of contractions/possessives is inherently fuzzy for
///   LLM output; strict count matching blocked ~40% of real production songs.
/// - v7: merge layer rewritten as pure Rust. Dropped the Claude call
///   entirely — its rules (base_confidence^2 weighting, >1000ms
///   disagreement handling, outlier rejection) are all deterministic math.
///   New algorithm: highest-base-confidence provider is primary, other
///   providers' timestamps (within 500ms) boost confidence to min(1.0,
///   base * 1.2); otherwise pass-through at base * 0.7. Zero stochastic
///   failure, zero API latency, identical output for non-failing songs.
/// - v8: sanitize word timings on the merge layer — enforce monotonic
///   start_ms, minimum per-word duration (80ms), and no overlap with
///   the next word's start. Fixes the 2026-04-19 event's blinking /
///   stuck karaoke display, which came from qwen3 emitting
///   zero-duration words, words that went backward in time, and
///   duplicate start_ms clusters.
/// - v9: sanitize runs on BOTH the single-provider pass-through and
///   the multi-provider merge. v8 had a gap: the single-provider
///   fast-path in `orchestrator.rs` copied qwen3's raw word timings
///   into the output without calling `sanitize_word_timings`, so
///   songs whose autosub failed (→ `ensemble:qwen3` bare) still
///   shipped with zero-duration / duplicate-start words. Measured
///   post-v8 on win-resolume: `ensemble:qwen3` songs had
///   duplicate_start_pct 20%+ while `ensemble:autosub+qwen3` songs
///   were 0%. v9 applies the same sanitizer everywhere.
/// - v10: sanitize threads `floor_start_ms` across line boundaries.
///   v9 sanitized WITHIN each line but reset the floor to 0 per
///   line, so two consecutive lines could have identical word
///   start_ms at their boundary. `compute_duplicate_start_pct`
///   sorts all starts globally and counts ties, so v9 audit logs
///   reported 91% duplicates even though per-line output was clean.
///   v10 makes cross-line boundaries strictly increasing too.
pub const LYRICS_PIPELINE_VERSION: u32 = 10;

/// Feature flag: enable the Gemini-based AlignmentProvider. When true, the
/// worker registers `GeminiProvider` in the provider list.
pub const LYRICS_GEMINI_ENABLED: bool = true;

/// Feature flag: enable the Qwen3 forced-alignment provider. When false, the
/// worker skips registering it even if Python venv is available. Kept as a
/// flag (not a code removal) so word-level work can revive qwen3 without a
/// history rewrite.
pub const LYRICS_QWEN3_ENABLED: bool = false;

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

    #[test]
    fn lyrics_pipeline_version_is_v10() {
        assert_eq!(
            LYRICS_PIPELINE_VERSION, 10,
            "version bump is the signal for catalog auto-reprocess; see CLAUDE.md history"
        );
    }

    #[test]
    fn gemini_enabled_and_qwen3_disabled_by_default() {
        assert!(super::LYRICS_GEMINI_ENABLED);
        assert!(!super::LYRICS_QWEN3_ENABLED);
    }
}
