pub mod aligner;
pub mod assembly;
pub mod audit_ctx;
pub mod backend;
pub mod bootstrap;
pub mod chunking;
pub mod claude_merge;
pub mod description_provider;
pub mod g35t_client;
pub mod g35t_transcript;
pub mod gather;
pub mod genius;
pub mod gpu_policy;
pub mod idle_gate;
pub mod line_splitter;
pub mod lrclib;
pub mod lyrics_ovh;
pub mod mtl_aligner;
pub mod orchestrator;
pub mod probe;
pub mod provider;
pub mod reference_gate;
pub mod renderer;
pub mod reprocess;
pub mod spotify_proxy;
pub mod spotify_resolver;
pub mod tier1;
pub mod translator;
pub mod worker;
pub mod worker_g35t;
pub mod worker_outcome;
pub mod worker_reference;
pub mod worker_translation;
pub mod youtube_subs;
pub use worker::LyricsWorker;
pub use worker::queue_update_loop;

use sp_core::lyrics::LyricsTrack;

/// Monotonic version of the lyrics pipeline output. Bump when prompts, the
/// provider/route registration, the alignment algorithm, or reference-text
/// selection changes. Every bump auto-reprocesses existing songs via the
/// stale-version bucket (`reprocess.rs`).
///
/// Condensed history (the full route archaeology lived here through v21; the
/// regimes below are all DELETED — kept only as a one-line trail):
/// - v1–v10: qwen3/autosub ensemble + Claude/Rust merge + word-timing
///   sanitizers (blinking-karaoke fixes). Ensemble deleted.
/// - v11–v18: Gemini chunked forced-alignment era — multi-key rotation,
///   CLIProxy↔direct-API flip-flops, the `lines: []` data-loss fix (v15),
///   AutoSub unregistered (v16), and the move to line-level-only timing
///   (v18, `words: None`, no synthesized word timings — still enforced).
///   Gemini chunked regime deleted.
/// - v19: manual yt_subs short-circuit. v20: Genius text source +
///   `lyrics_override_text`; sole aligner became WhisperX-on-Replicate with
///   an AssemblyAI-U3-Pro `asr_path` fallback for no-text songs.
/// - v21 (#143): Lever-2 reference regime added — `mtl_aligner` force-align
///   verified by a Gemini-3.5-Transcribe word transcript (`reference_gate`),
///   ship ★ on gate PASS, else fall through to the v20 WhisperX/asr_path
///   routes.
/// - v22 (#159): **one regime.** The v20 WhisperX-on-Replicate route
///   (`whisperx_replicate`/`Orchestrator`) and the AssemblyAI `asr_path`
///   route are DELETED, not kept as fallbacks (owner directive 2026-09-14).
///   The pipeline is now two tiers, both anchored on the v21 tooling:
///   (1) ★ tier — text candidate (≥4 lines) + vocals → `mtl_aligner` force-
///   align + `reference_gate` g35t verify → PASS ships mtl line timings
///   (`<src>+mtl@rev1/g35t-ok`, `videos.lyrics_reference=1`); UNCHANGED from
///   v21. (2) base tier — everything else (no usable text, gate fail, mtl
///   skip/error) → a Gemini-3.5-Transcribe transcript grouped into lines
///   (`g35t_transcript`, source `gemini-3-5-transcribe`). One forced
///   aligner (mtl), one ASR vendor (Gemini). Measured no-text quality:
///   g35t 19.7% gold-norm ≤400ms vs the retired AssemblyAI 3.8%
///   (`eval/lyrics/reports/2026-09-12-gemini-3-5-transcribe.md`). Every
///   pre-v22 row re-queues (no smart-skip — that was the v18 trap); v21
///   mtl rows re-run to identical output and re-★.
pub const LYRICS_PIPELINE_VERSION: u32 = 22;

/// Monotonic version of the SK **translation** output (#152), INDEPENDENT of
/// `LYRICS_PIPELINE_VERSION`. Bump ONLY when the translation prompt changes in
/// a way that alters the Slovak wording (e.g. the gender framing added in
/// #152). A bump re-translates existing songs — one Claude call each, the `sk`
/// lines rewritten in place by the same JSON writer the pipeline uses — via the
/// stale-translation selector in `models_translation::fetch_next_stale_translation`,
/// and NEVER re-runs alignment or touches `lyrics_pipeline_version`. Every
/// `videos` row starts at `lyrics_translation_version = 0`, so the first
/// non-zero value re-translates the whole catalog under the current prompt.
///
/// History:
/// - v1 (#152): gender-aware grandparent/plaque framing.
/// - v2 (#145): the grandparent/plaque STORY is replaced by a neutral technical
///   prompt (`translator::build_prompt`) — the newest flagships refused the
///   story ("…even for a family plaque…") while the neutral framing translates
///   every measured song and preserves masculine/feminine forms. The Slovak
///   output changes (new framing + previously-refused songs now translate), so
///   the catalog re-translates under the working prompt. No alignment change →
///   `LYRICS_PIPELINE_VERSION` untouched.
pub const LYRICS_TRANSLATION_VERSION: u32 = 2;

/// Upper duration bound for lyrics processing (#144, "Rollout blocker #3").
///
/// A row longer than this is not a real song: the catalog's five > 30-min
/// videos (36–70 min) are live sets / mixes with no single lyric sheet, and
/// each retry of one is a full ~1 h GPU burn on the shared live PC (isolation
/// runs at ≈1× realtime). The longest actual song is 21 min and is already
/// aligned. `process_song` stamps any row over this cap `unsupported_source`
/// before any gather/network/GPU work runs; an operator can still force one
/// with `lyrics_override_text` + manual priority if ever needed.
pub const MAX_LYRICS_DURATION_MS: i64 = 30 * 60 * 1000;

/// Alignment-model identifier written to `lyrics_alignment_model` when no
/// forced alignment ran (defensive default for a raw line-timed ship-through;
/// still referenced by `db::models`).
pub const ALIGNMENT_MODEL_NONE: &str = "none";

/// Alignment-model literal stamped on lyrics rows produced by the Lever-2
/// (#143) forced-alignment reference stage: `lyrics-alignment-mtl`
/// (MTL+BDR) timing verified by an independent Gemini 3.5 Transcribe word
/// transcript. `rev1` bumps when the mtl subprocess wrapper or the gate
/// thresholds change in a way that affects production output.
pub const ALIGNMENT_MODEL_MTL_REV1: &str = "lyrics-alignment-mtl@rev1";

/// Alignment-model literal stamped on lyrics rows produced by the v22 (#159)
/// base tier: a Gemini 3.5 Transcribe transcript grouped into lines
/// (`g35t_transcript`). `rev1` bumps when the grouping/gap thresholds change
/// in a way that affects production output.
pub const ALIGNMENT_MODEL_G35T_REV1: &str = "gemini-3-5-transcribe@rev1";

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
    fn lyrics_pipeline_version_is_v22() {
        assert_eq!(
            LYRICS_PIPELINE_VERSION, 22,
            "v22 = one regime: mtl ★ tier + g35t base tier, legacy routes deleted (#159)"
        );
    }

    #[test]
    fn lyrics_translation_version_is_v2() {
        assert_eq!(
            LYRICS_TRANSLATION_VERSION, 2,
            "v2 = neutral technical translation prompt replaces the #152 grandparent/plaque story (#145)"
        );
        assert!(
            LYRICS_TRANSLATION_VERSION != LYRICS_PIPELINE_VERSION,
            "translation version is independent of the pipeline version"
        );
    }
}

#[cfg(test)]
mod probe_tests;

#[cfg(test)]
#[path = "canonical_source_regression_tests.rs"]
mod canonical_source_regression_tests;

#[cfg(test)]
#[path = "worker_tests_duration_cap.rs"]
mod worker_tests_duration_cap;
