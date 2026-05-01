//! Tier-1 — free text + line-timing fetchers.
//!
//! `pick_best` is pure logic: short-circuits when any candidate has
//! `has_timing=true` AND at least `TIER1_MIN_LINES` lines. Else returns
//! `TextOnly` for downstream Tier-2 (WhisperX) + reconciliation. Else
//! `None` if no fetcher returned anything usable.
//!
//! `collect` is the async wrapper that runs all configured fetchers in
//! parallel via `futures::future::join_all` and feeds successful (`Some`)
//! results into `pick_best`.
//!
//! Per `feedback_line_timing_only.md`: Tier-1 line-synced output ships
//! `words: None` on every `AlignedLine` — the renderer falls back to
//! line-level highlighting. Word timings are NEVER synthesized.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::lyrics::backend::AlignedLine;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateText {
    /// "tier1:spotify" / "tier1:lrclib" / "tier1:yt_subs" / "genius" etc.
    pub source: String,
    pub lines: Vec<String>,
    /// `Some` when the fetcher has line-level timing. `start_ms`, `end_ms` per line.
    pub line_timings: Option<Vec<(u64, u64)>>,
    pub has_timing: bool,
}

// Temporary bridge — Phase G deletes provider.rs and this impl with it.
// Reverse direction lives in `provider.rs` next to its struct.
impl From<crate::lyrics::provider::CandidateText> for CandidateText {
    fn from(c: crate::lyrics::provider::CandidateText) -> Self {
        Self {
            source: c.source,
            lines: c.lines,
            line_timings: c.line_timings,
            has_timing: c.has_timing,
        }
    }
}

/// Threshold for Tier-1 short-circuit: only ship directly if the source
/// has timing AND at least this many lines. Below this, treat as
/// suspiciously short (intro snippet, partial fetch, etc.) and fall
/// through to Tier-2 + reconciliation.
pub const TIER1_MIN_LINES: usize = 10;

#[derive(Debug, Clone)]
pub enum Tier1Result {
    /// One source has line-synced authoritative output. Ship directly.
    LineSynced(AlignedLines),
    /// Only text-only candidates (no timing). Pass to Tier-2 + reconcile.
    TextOnly(Vec<CandidateText>),
    /// No fetchers returned anything usable.
    None,
}

#[derive(Debug, Clone)]
pub struct AlignedLines {
    pub lines: Vec<AlignedLine>,
    pub provenance: String,
}

/// Per-fetcher async closure shape so the collector doesn't depend on
/// concrete fetcher types — keeps Phase G's deletion of provider.rs clean.
pub type FetchFn =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, Option<CandidateText>> + Send + Sync>;

/// Pure logic that picks the best Tier-1 candidate from a vec.
/// Broken out for unit testability — wire-up tests don't need real HTTP.
///
/// Rule: first candidate (in input order) with `has_timing == true` AND
/// `lines.len() >= TIER1_MIN_LINES` AND `line_timings.is_some()` →
/// `LineSynced`. Else if any non-empty candidates exist → `TextOnly`.
/// Else `None`.
pub fn pick_best(candidates: Vec<CandidateText>) -> Tier1Result {
    for c in &candidates {
        if c.has_timing && c.lines.len() >= TIER1_MIN_LINES {
            if let Some(timings) = &c.line_timings {
                if timings.len() == c.lines.len() {
                    let aligned: Vec<AlignedLine> = c
                        .lines
                        .iter()
                        .zip(timings.iter())
                        .map(|(text, (start, end))| AlignedLine {
                            text: text.clone(),
                            start_ms: *start as u32,
                            end_ms: *end as u32,
                            // Per feedback_line_timing_only.md: never synthesize
                            // word timings. Tier-1 line-synced ships words: None;
                            // renderer falls back to line-level highlighting.
                            words: None,
                        })
                        .collect();
                    return Tier1Result::LineSynced(AlignedLines {
                        lines: aligned,
                        provenance: c.source.clone(),
                    });
                }
            }
        }
    }
    if candidates.is_empty() {
        Tier1Result::None
    } else {
        Tier1Result::TextOnly(candidates)
    }
}

/// Collect from all configured fetchers in parallel, then pick the best.
///
/// `fetchers` is a list of FetchFn closures; the caller (orchestrator)
/// constructs each closure with its captured fetcher + per-song args
/// (artist, track, duration, spotify_track_id, vtt_path). Failed fetchers
/// (None) are silently dropped — the rest of the chain proceeds.
pub async fn collect(fetchers: Vec<FetchFn>) -> Tier1Result {
    let futures: Vec<_> = fetchers.iter().map(|f| f()).collect();
    let results: Vec<_> = futures::future::join_all(futures).await;
    let candidates: Vec<CandidateText> = results.into_iter().flatten().collect();
    pick_best(candidates)
}

/// Convert an sp-core `LyricsTrack` into a `tier1::CandidateText`.
///
/// Used by Phase F orchestrator wiring: when wrapping `lrclib`, `genius`,
/// `youtube_subs` (and any future Tier-1 fetcher that emits `LyricsTrack`)
/// into a `FetchFn` closure, the closure body invokes the fetcher and then
/// passes the `LyricsTrack` through this adapter.
///
/// `has_timing` is `true` iff at least one line has a non-zero timestamp.
/// All-zero-timing tracks (e.g., plain Genius text) return
/// `line_timings: None` and `has_timing: false`.
pub fn lyrics_track_to_candidate(track: sp_core::lyrics::LyricsTrack) -> CandidateText {
    let any_timing = track.lines.iter().any(|l| l.start_ms != 0 || l.end_ms != 0);
    let mut lines: Vec<String> = Vec::with_capacity(track.lines.len());
    let mut timings: Vec<(u64, u64)> = Vec::with_capacity(track.lines.len());
    for l in track.lines {
        timings.push((l.start_ms, l.end_ms));
        lines.push(l.en);
    }
    CandidateText {
        source: track.source,
        lines,
        line_timings: if any_timing { Some(timings) } else { None },
        has_timing: any_timing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sp_core::lyrics::{LyricsLine, LyricsTrack};

    fn cand(
        source: &str,
        lines_count: usize,
        has_timing: bool,
        with_timings: bool,
    ) -> CandidateText {
        let lines: Vec<String> = (0..lines_count).map(|i| format!("line {i}")).collect();
        let timings: Option<Vec<(u64, u64)>> = if with_timings {
            Some(
                (0..lines_count)
                    .map(|i| (i as u64 * 1000, i as u64 * 1000 + 1000))
                    .collect(),
            )
        } else {
            None
        };
        CandidateText {
            source: source.into(),
            lines,
            line_timings: timings,
            has_timing,
        }
    }

    #[test]
    fn tier1_min_lines_is_ten() {
        assert_eq!(TIER1_MIN_LINES, 10);
    }

    #[test]
    fn line_synced_above_threshold_ships_directly() {
        let r = pick_best(vec![cand("tier1:spotify", 12, true, true)]);
        match r {
            Tier1Result::LineSynced(a) => {
                assert_eq!(a.lines.len(), 12);
                assert_eq!(a.provenance, "tier1:spotify");
                // feedback_line_timing_only.md: ship words: None
                for l in &a.lines {
                    assert!(l.words.is_none(), "Tier-1 ships words: None");
                }
            }
            _ => panic!("expected LineSynced"),
        }
    }

    #[test]
    fn line_synced_below_threshold_falls_through_to_text_only() {
        let r = pick_best(vec![cand("tier1:spotify", 5, true, true)]);
        match r {
            Tier1Result::TextOnly(v) => assert_eq!(v.len(), 1),
            _ => panic!("expected TextOnly fallthrough"),
        }
    }

    #[test]
    fn line_synced_with_mismatched_timing_count_falls_through() {
        // 12 lines but only 5 timings — invariant violation, fall through
        let mut c = cand("tier1:spotify", 12, true, true);
        c.line_timings = Some(vec![(0, 1000); 5]);
        let r = pick_best(vec![c]);
        assert!(matches!(r, Tier1Result::TextOnly(_)));
    }

    #[test]
    fn text_only_candidate_returns_text_only_variant() {
        let r = pick_best(vec![cand("genius", 30, false, false)]);
        assert!(matches!(r, Tier1Result::TextOnly(_)));
    }

    #[test]
    fn empty_candidates_returns_none() {
        let r = pick_best(vec![]);
        assert!(matches!(r, Tier1Result::None));
    }

    #[test]
    fn first_line_synced_wins_over_later_text_only() {
        let r = pick_best(vec![
            cand("tier1:spotify", 12, true, true),
            cand("genius", 30, false, false),
        ]);
        match r {
            Tier1Result::LineSynced(a) => assert_eq!(a.provenance, "tier1:spotify"),
            _ => panic!("first line-synced should win"),
        }
    }

    #[test]
    fn text_only_first_then_line_synced_still_picks_line_synced() {
        // Order doesn't matter for line-synced detection — the loop scans all.
        let r = pick_best(vec![
            cand("genius", 30, false, false),
            cand("tier1:spotify", 12, true, true),
        ]);
        match r {
            Tier1Result::LineSynced(a) => assert_eq!(a.provenance, "tier1:spotify"),
            _ => panic!("any line-synced should win"),
        }
    }

    #[tokio::test]
    async fn collect_with_zero_fetchers_returns_none() {
        let r = collect(vec![]).await;
        assert!(matches!(r, Tier1Result::None));
    }

    #[tokio::test]
    async fn collect_with_one_async_fetcher_returns_text_only() {
        let f: FetchFn = Arc::new(|| {
            Box::pin(async {
                Some(CandidateText {
                    source: "test".into(),
                    lines: vec!["a".into(), "b".into()],
                    line_timings: None,
                    has_timing: false,
                })
            })
        });
        let r = collect(vec![f]).await;
        assert!(matches!(r, Tier1Result::TextOnly(_)));
    }

    #[tokio::test]
    async fn collect_with_failing_fetcher_drops_silently() {
        let none_fetcher: FetchFn = Arc::new(|| Box::pin(async { None }));
        let good_fetcher: FetchFn = Arc::new(|| {
            Box::pin(async {
                Some(CandidateText {
                    source: "test".into(),
                    lines: vec!["a".into()],
                    line_timings: None,
                    has_timing: false,
                })
            })
        });
        let r = collect(vec![none_fetcher, good_fetcher]).await;
        assert!(matches!(r, Tier1Result::TextOnly(v) if v.len() == 1));
    }

    fn lt_line(en: &str, start: u64, end: u64) -> LyricsLine {
        LyricsLine {
            start_ms: start,
            end_ms: end,
            en: en.into(),
            sk: None,
            words: None,
        }
    }

    fn lt(source: &str, lines: Vec<LyricsLine>) -> LyricsTrack {
        LyricsTrack {
            version: 1,
            source: source.into(),
            language_source: "en".into(),
            language_translation: String::new(),
            lines,
        }
    }

    #[test]
    fn lyrics_track_with_timings_yields_has_timing_true() {
        let track = lt(
            "lrclib",
            vec![lt_line("hello", 0, 1000), lt_line("world", 1000, 2000)],
        );
        let c = lyrics_track_to_candidate(track);
        assert_eq!(c.source, "lrclib");
        assert_eq!(c.lines, vec!["hello", "world"]);
        assert!(c.has_timing);
        assert_eq!(
            c.line_timings.as_ref().unwrap(),
            &vec![(0, 1000), (1000, 2000)]
        );
    }

    #[test]
    fn lyrics_track_with_all_zero_timings_yields_has_timing_false() {
        // Plain Genius text — no timings on any line
        let track = lt(
            "genius",
            vec![lt_line("hello", 0, 0), lt_line("world", 0, 0)],
        );
        let c = lyrics_track_to_candidate(track);
        assert_eq!(c.source, "genius");
        assert_eq!(c.lines, vec!["hello", "world"]);
        assert!(!c.has_timing);
        assert!(c.line_timings.is_none());
    }

    #[test]
    fn lyrics_track_with_at_least_one_nonzero_timing_yields_has_timing_true() {
        // Mixed: first line zero, second line has timing → still treated as
        // "has timing" since SOME line is timed. Caller (pick_best) further
        // checks line_timings.len() == lines.len().
        let track = lt(
            "yt_subs",
            vec![lt_line("hello", 0, 0), lt_line("world", 1000, 2000)],
        );
        let c = lyrics_track_to_candidate(track);
        assert!(c.has_timing);
        // Both timings emitted (first one is (0, 0) but the count matches lines)
        let timings = c.line_timings.as_ref().unwrap();
        assert_eq!(timings.len(), 2);
        assert_eq!(timings[0], (0, 0));
        assert_eq!(timings[1], (1000, 2000));
    }

    #[test]
    fn lyrics_track_empty_lines_yields_empty_candidate() {
        let track = lt("lrclib", vec![]);
        let c = lyrics_track_to_candidate(track);
        assert_eq!(c.source, "lrclib");
        assert!(c.lines.is_empty());
        assert!(!c.has_timing);
        assert!(c.line_timings.is_none());
    }

    // ── lyrics_track_to_candidate: || vs && in any_timing (line 136 mutant) ──
    //
    // Mutant: `||` → `&&` — under `&&`, a line with `start_ms != 0` but
    // `end_ms == 0` would NOT count as having timing (1000 != 0 && 0 != 0 = false).
    // Correct code uses `||`: either nonzero field is enough to flag timing.
    //
    // Test: a line with start_ms=1000 and end_ms=0 MUST produce has_timing=true.

    #[test]
    fn any_timing_detected_when_only_start_ms_nonzero() {
        // start_ms=1000, end_ms=0 — only start is nonzero.
        // With `||`: 1000!=0 || 0!=0 = true → has_timing=true.
        // With `&&` mutant: 1000!=0 && 0!=0 = false → has_timing=false (WRONG).
        let track = lt("tier1:lrclib", vec![lt_line("Amazing grace", 1000, 0)]);
        let c = lyrics_track_to_candidate(track);
        assert!(
            c.has_timing,
            "start_ms=1000 alone must set has_timing=true (|| not &&)"
        );
        assert!(
            c.line_timings.is_some(),
            "line_timings must be Some when has_timing is true"
        );
    }

    #[test]
    fn any_timing_detected_when_only_end_ms_nonzero() {
        // start_ms=0, end_ms=2000 — only end is nonzero.
        // With `||`: 0!=0 || 2000!=0 = true → has_timing=true.
        // With `&&` mutant: 0!=0 && 2000!=0 = false → has_timing=false (WRONG).
        let track = lt(
            "tier1:spotify",
            vec![lt_line("How sweet the sound", 0, 2000)],
        );
        let c = lyrics_track_to_candidate(track);
        assert!(
            c.has_timing,
            "end_ms=2000 alone must set has_timing=true (|| not &&)"
        );
        assert!(c.line_timings.is_some());
    }
}
