//! Turning a fetched lyric into a reference-text candidate — shared by the
//! artist+title lookups (`gather.rs`) and the title search (#144,
//! `title_search.rs`), so both build a candidate the same way.
//!
//! A TIMED source (LRCLIB synced lyrics, YouTube captions) is used as is. A
//! plain scraped text (LRCLIB plain lyrics, a Genius page — whose section
//! labels and contributor banner `genius::extract_lyrics_from_html` already
//! dropped) first goes through the Claude cleanup that drops ad-libs and
//! hype intros and keeps every sung repeat
//! (`description_provider::clean_lyrics_via_claude`, `ScrapedLyrics` mode).

use std::path::Path;

use sp_core::lyrics::LyricsTrack;

use crate::lyrics::provider::CandidateText;

/// A timed candidate: the track's lines with their own line timings.
pub(crate) fn timed_candidate(source: &str, track: &LyricsTrack) -> CandidateText {
    CandidateText {
        source: source.to_string(),
        lines: track.lines.iter().map(|l| l.en.clone()).collect(),
        has_timing: true,
        line_timings: Some(track.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
    }
}

/// A text-only candidate from a scraped plain lyric, cleaned by Claude.
/// `Ok(None)` when the cleanup finds no lyric in it. The cleanup result is
/// cached at `cache_path` (a cached decision is reused without a call).
pub(crate) async fn cleaned_text_candidate(
    ai: &crate::ai::client::AiClient,
    song: &str,
    artist: &str,
    source: &str,
    track: &LyricsTrack,
    cache_path: &Path,
) -> anyhow::Result<Option<CandidateText>> {
    let raw_blob: String = track
        .lines
        .iter()
        .map(|l| l.en.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let cleaned = crate::lyrics::description_provider::clean_lyrics_via_claude(
        ai,
        song,
        artist,
        &raw_blob,
        cache_path,
        crate::lyrics::description_provider::CleanupMode::ScrapedLyrics,
    )
    .await?;
    Ok(cleaned
        .filter(|lines| !lines.is_empty())
        .map(|lines| CandidateText {
            source: source.to_string(),
            lines,
            has_timing: false,
            line_timings: None,
        }))
}

#[cfg(test)]
#[path = "text_candidate_tests.rs"]
mod tests;
