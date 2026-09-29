//! #144 — a cover's reference text, found by TITLE and chosen by what is
//! SUNG.
//!
//! Every artist+title lookup (`gather.rs`: LRCLIB `/api/get`, lyrics.ovh,
//! Genius) searches the row's metadata artist, and for a cover that is the
//! COVER artist: 158 "Jesus Be the Name" by CityHill Worship finds nothing,
//! while LRCLIB and Genius hold the original under Elevation Worship. So
//! when those lookups found nothing, the worker searches by the title alone
//! (`lrclib_search`, `genius::search_by_title_at`), scores every candidate
//! against the song's one Gemini 3.5 transcript by word overlap, and takes
//! the best above a measured floor as the reference text. The normal mtl +
//! two-way gate then verifies it against the audio again. No LLM guesses the
//! original artist: the audio decides.

use std::collections::HashMap;
use std::path::Path;

use sp_core::lyrics::LyricsTrack;
use tracing::{info, warn};

use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::provider::CandidateText;
use crate::lyrics::reference_gate::{normalize_word, normalized_words};

/// A title candidate's word overlap with the transcript must reach this to
/// become the reference text. Measured on #144 (issue comment 5899043518):
/// a song's own lyric scores 0.664–0.951 against its transcript, the best
/// OTHER song 0.379 at most (18 eval songs × 17 others); live, 158's Genius
/// Elevation Worship page scores 0.657 and the other songs titled "I Will Go"
/// score 0.17–0.32 against 286.
pub const MIN_TITLE_MATCH_SCORE: f64 = 0.50;

/// Where a title candidate came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleProvider {
    Lrclib,
    Genius,
}

impl TitleProvider {
    /// The candidate source label — the same slot as the provider's
    /// artist+title lookup in `claude_merge::priority_with_timing`.
    pub fn source(self) -> &'static str {
        match self {
            TitleProvider::Lrclib => "lrclib",
            TitleProvider::Genius => "genius",
        }
    }
}

/// One lyric found by title.
#[derive(Debug, Clone, PartialEq)]
pub struct TitleCandidate {
    pub provider: TitleProvider,
    /// LRCLIB record id, or the Genius page URL.
    pub id: String,
    pub artist: String,
    /// The LRCLIB record's duration (Genius has none).
    pub record_duration_s: Option<f32>,
    pub lyrics: LyricsTrack,
    /// LRCLIB synced lyrics (a timed candidate, used as is); otherwise the
    /// text is a scraped plain lyric that goes through the Claude cleanup.
    pub synced: bool,
}

impl From<crate::lyrics::lrclib_search::LrclibTitleHit> for TitleCandidate {
    fn from(h: crate::lyrics::lrclib_search::LrclibTitleHit) -> Self {
        Self {
            provider: TitleProvider::Lrclib,
            id: h.id.to_string(),
            artist: format!("{} — {}", h.artist, h.track),
            record_duration_s: h.duration_s,
            lyrics: h.lyrics,
            synced: h.synced,
        }
    }
}

impl From<crate::lyrics::genius::GeniusTitleHit> for TitleCandidate {
    fn from(h: crate::lyrics::genius::GeniusTitleHit) -> Self {
        Self {
            provider: TitleProvider::Genius,
            id: h.url,
            artist: h.artist,
            record_duration_s: None,
            lyrics: h.lyrics,
            synced: false,
        }
    }
}

/// True when the title search should run: none of the gathered candidates
/// came from an artist+title lookup (LRCLIB, lyrics.ovh / Genius — both
/// labelled `genius`) or from an exact-song source (the operator override,
/// a Spotify track id). YouTube captions and the description do not count:
/// they can be partial (song 286's description held a third of the lyric).
pub fn needs_title_search(candidates: &[crate::lyrics::tier1::CandidateText]) -> bool {
    !candidates.iter().any(|c| {
        matches!(c.source.as_str(), "lrclib" | "genius" | "override")
            || c.source.starts_with("tier1:spotify")
    })
}

fn word_counts(words: impl Iterator<Item = String>) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for w in words {
        *counts.entry(w).or_insert(0) += 1;
    }
    counts
}

/// Word overlap of a lyric with the sung transcript: the multiset F1 (Dice)
/// of the normalized words — 2 × shared ÷ (text words + sung words). Order
/// plays no part; a same-title different song shares only common words.
pub fn overlap_score(lyrics: &LyricsTrack, words: &[AsrWord]) -> f64 {
    let text = word_counts(lyrics.lines.iter().flat_map(|l| normalized_words(&l.en)));
    let sung = word_counts(
        words
            .iter()
            .map(|w| normalize_word(&w.text))
            .filter(|w| !w.is_empty()),
    );
    let shared: usize = text
        .iter()
        .map(|(w, n)| (*n).min(sung.get(w).copied().unwrap_or(0)))
        .sum();
    if shared == 0 {
        return 0.0;
    }
    let total: usize = text.values().sum::<usize>() + sung.values().sum::<usize>();
    2.0 * shared as f64 / total as f64
}

/// The index of the best score at or above `MIN_TITLE_MATCH_SCORE`; on a tie
/// the earlier candidate (LRCLIB before Genius, then the search's own order).
pub fn pick_best(scores: &[f64]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, &score) in scores.iter().enumerate() {
        if score >= MIN_TITLE_MATCH_SCORE && best.is_none_or(|b| score > scores[b]) {
            best = Some(i);
        }
    }
    best
}

/// The cache file of the Claude cleanup of a chosen plain candidate, keyed by
/// the candidate itself (a later search may choose another lyric) and kept
/// apart from the artist+title lookups' `{yt}_lrclib_cleaned_v2.json`.
pub fn cleanup_cache_name(youtube_id: &str, cand: &TitleCandidate) -> String {
    let key: String = cand
        .id
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(80)
        .collect();
    format!(
        "{youtube_id}_title_{}_{key}_cleaned_v2.json",
        cand.provider.source()
    )
}

/// The `{yt}_title_search_audit.json` record: every candidate scored, the
/// floor and the one chosen (`null` when none reached the floor).
pub fn audit_json(
    title: &str,
    duration_s: Option<u32>,
    cands: &[TitleCandidate],
    scores: &[f64],
    chosen: Option<usize>,
) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = cands
        .iter()
        .zip(scores)
        .map(|(c, score)| {
            serde_json::json!({
                "provider": c.provider.source(),
                "id": c.id,
                "artist": c.artist,
                "record_duration_s": c.record_duration_s,
                "lines": c.lyrics.lines.len(),
                "synced": c.synced,
                "score": score,
            })
        })
        .collect();
    serde_json::json!({
        "title": title,
        "duration_s": duration_s,
        "min_score": MIN_TITLE_MATCH_SCORE,
        "candidates": rows,
        "chosen": chosen,
    })
}

/// The search endpoints (production, or a wiremock server in tests).
pub struct TitleSearchEndpoints {
    pub lrclib_search: String,
    pub genius_search: String,
}

impl TitleSearchEndpoints {
    pub fn production() -> Self {
        Self {
            lrclib_search: crate::lyrics::lrclib_search::LRCLIB_SEARCH_URL.to_string(),
            genius_search: crate::lyrics::genius::GENIUS_SEARCH_URL.to_string(),
        }
    }
}

/// The inputs of one title search.
pub struct TitleSearch<'a> {
    pub client: &'a reqwest::Client,
    /// The Claude client for the cleanup of a plain lyric (`None`: only a
    /// synced LRCLIB lyric can be used).
    pub ai: Option<&'a crate::ai::client::AiClient>,
    pub genius_token: &'a str,
    pub cache_dir: &'a Path,
}

impl TitleSearch<'_> {
    /// Search both providers by `row.song`, score every candidate against
    /// `words`, write the audit, and return the chosen lyric as a candidate
    /// (`None` when no title, nothing reaches the floor, or the cleanup of
    /// the chosen plain lyric finds none). Every failure is logged, never
    /// fatal: the song keeps its gathered candidates.
    pub async fn find(
        &self,
        endpoints: &TitleSearchEndpoints,
        row: &crate::db::models::VideoLyricsRow,
        words: &[AsrWord],
    ) -> Option<CandidateText> {
        let title = row.song.trim();
        if title.is_empty() {
            return None;
        }
        let youtube_id = row.youtube_id.as_str();
        let duration_s = row.duration_ms.map(|ms| (ms / 1000) as u32);

        let mut cands: Vec<TitleCandidate> = Vec::new();
        match crate::lyrics::lrclib_search::search_by_title_at(
            self.client,
            &endpoints.lrclib_search,
            title,
            duration_s,
        )
        .await
        {
            Ok(hits) => cands.extend(hits.into_iter().map(TitleCandidate::from)),
            Err(e) => warn!(%youtube_id, error = %e, "title search: LRCLIB failed"),
        }
        match crate::lyrics::genius::search_by_title_at(
            self.client,
            &endpoints.genius_search,
            self.genius_token,
            title,
        )
        .await
        {
            Ok(hits) => cands.extend(hits.into_iter().map(TitleCandidate::from)),
            Err(e) => warn!(%youtube_id, error = %e, "title search: Genius failed"),
        }

        let scores: Vec<f64> = cands
            .iter()
            .map(|c| overlap_score(&c.lyrics, words))
            .collect();
        let chosen = pick_best(&scores);
        let audit = audit_json(title, duration_s, &cands, &scores, chosen);
        let audit_path = self
            .cache_dir
            .join(format!("{youtube_id}_title_search_audit.json"));
        if let Err(e) = tokio::fs::write(&audit_path, audit.to_string()).await {
            warn!(path = %audit_path.display(), %e, "title search: audit write failed");
        }

        let Some(i) = chosen else {
            info!(%youtube_id, title, candidates = cands.len(), "title search: no lyric reaches the floor");
            return None;
        };
        let cand = &cands[i];
        info!(
            %youtube_id,
            title,
            provider = cand.provider.source(),
            id = %cand.id,
            artist = %cand.artist,
            score = scores[i],
            "title search: chose a lyric (#144)"
        );
        self.candidate_from(cand, row).await
    }

    /// The chosen lyric as a reference-text candidate: a synced LRCLIB lyric
    /// as is, a plain one through the Claude cleanup.
    async fn candidate_from(
        &self,
        cand: &TitleCandidate,
        row: &crate::db::models::VideoLyricsRow,
    ) -> Option<CandidateText> {
        let source = cand.provider.source();
        if cand.synced {
            return Some(crate::lyrics::text_candidate::timed_candidate(
                source,
                &cand.lyrics,
            ));
        }
        let Some(ai) = self.ai else {
            warn!(youtube_id = %row.youtube_id, "title search: no Claude client for the cleanup");
            return None;
        };
        let cache_path = self
            .cache_dir
            .join(cleanup_cache_name(&row.youtube_id, cand));
        match crate::lyrics::text_candidate::cleaned_text_candidate(
            ai,
            &row.song,
            &row.artist,
            source,
            &cand.lyrics,
            &cache_path,
        )
        .await
        {
            Ok(found) => found,
            Err(e) => {
                warn!(youtube_id = %row.youtube_id, error = %e, "title search: cleanup failed");
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "title_search_tests.rs"]
mod tests;
