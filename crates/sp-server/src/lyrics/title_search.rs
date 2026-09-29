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
//! the best above a measured floor. That lyric becomes the reference text
//! unless the video's own gathered text (its captions or description)
//! matches what is sung at least as well (`choose_reference`) — a partial
//! description loses to the full lyric, the video's own complete captions
//! are not displaced by another recording's lyric. The normal mtl + two-way
//! gate then verifies the reference against the audio again. No LLM guesses
//! the original artist: the audio decides.

use std::collections::HashMap;
use std::path::Path;

use sp_core::lyrics::LyricsTrack;
use tracing::{info, warn};

use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::provider::CandidateText;
use crate::lyrics::reference_gate::{normalize_word, normalized_words};
use crate::lyrics::tier1::CandidateText as TierCandidate;

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
    lines_overlap_score(lyrics.lines.iter().map(|l| l.en.as_str()), words)
}

/// `overlap_score` of any lines of text (a gathered candidate's lines).
pub fn lines_overlap_score<'a>(lines: impl Iterator<Item = &'a str>, words: &[AsrWord]) -> f64 {
    let text = word_counts(lines.flat_map(normalized_words));
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

/// The indices of the scores at or above `MIN_TITLE_MATCH_SCORE`, best
/// first; a tie keeps the earlier candidate first (LRCLIB before Genius,
/// then the search's own order). The caller takes the first one whose lyric
/// it can use (a plain lyric the Claude cleanup rejects passes to the next).
pub fn rank_above_floor(scores: &[f64]) -> Vec<usize> {
    let mut ranked: Vec<usize> = (0..scores.len())
        .filter(|&i| scores[i] >= MIN_TITLE_MATCH_SCORE)
        .collect();
    // `sort_by` is stable: equal scores keep their index order.
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
    ranked
}

/// Which text goes to mtl once the title search found a lyric scoring
/// `title_score`: `Some(i)` = the gathered text `i` (the video's own
/// captions / description) when it matches what is sung at least as well
/// (a tie keeps the video's own text, and the earlier gathered text);
/// `None` = the title lyric.
pub fn choose_reference(gathered_scores: &[f64], title_score: f64) -> Option<usize> {
    let mut best: Option<usize> = None;
    let mut best_score = title_score;
    for (i, &score) in gathered_scores.iter().enumerate() {
        let beats = match best {
            None => score >= best_score,
            Some(_) => score > best_score,
        };
        if beats {
            best = Some(i);
            best_score = score;
        }
    }
    best
}

/// The audit path of the title search.
fn audit_path(cache_dir: &Path, youtube_id: &str) -> std::path::PathBuf {
    cache_dir.join(format!("{youtube_id}_title_search_audit.json"))
}

/// Remove a title-search audit left by an earlier run when this run does not
/// search (an artist+title lookup found the song now), so the file on disk
/// always describes the latest run. Best-effort.
pub async fn remove_audit(cache_dir: &Path, youtube_id: &str) {
    let _ = tokio::fs::remove_file(audit_path(cache_dir, youtube_id)).await;
}

/// The cache file of the Claude cleanup of a chosen plain candidate, keyed by
/// the candidate itself (a later search may choose another lyric) and kept
/// apart from the artist+title lookups' `{yt}_lrclib_cleaned_v3.json`.
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
        "{youtube_id}_title_{}_{key}_cleaned_v3.json",
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
    /// `words`, take the best usable one at or above the floor, and return
    /// the REFERENCE text for mtl: that lyric, or a `gathered` text of the
    /// video itself that matches what is sung at least as well
    /// (`choose_reference`). `None` when there is no title or no usable lyric
    /// reaches the floor — the caller then picks from `gathered` by source
    /// priority as before. Writes `{yt}_title_search_audit.json`. Every
    /// failure is logged, never fatal.
    pub async fn find(
        &self,
        endpoints: &TitleSearchEndpoints,
        row: &crate::db::models::VideoLyricsRow,
        words: &[AsrWord],
        gathered: &[TierCandidate],
    ) -> Option<TierCandidate> {
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
        let mut found: Option<(usize, TierCandidate)> = None;
        for i in rank_above_floor(&scores) {
            if let Some(c) = self.candidate_from(&cands[i], row).await {
                found = Some((i, c.into()));
                break;
            }
        }

        // The found lyric (as cleaned) against the video's own texts.
        let gathered_scores: Vec<f64> = gathered
            .iter()
            .map(|c| lines_overlap_score(c.lines.iter().map(String::as_str), words))
            .collect();
        let reference = found.as_ref().map(|(_, lyric)| {
            let lyric_score = lines_overlap_score(lyric.lines.iter().map(String::as_str), words);
            let pick = choose_reference(&gathered_scores, lyric_score);
            let chosen = pick.map_or(lyric, |g| &gathered[g]);
            (chosen.clone(), lyric_score, pick)
        });

        let mut audit = audit_json(
            title,
            duration_s,
            &cands,
            &scores,
            found.as_ref().map(|(i, _)| *i),
        );
        audit["gathered"] = serde_json::json!(
            gathered
                .iter()
                .zip(&gathered_scores)
                .map(|(c, s)| serde_json::json!({"source": c.source, "score": s}))
                .collect::<Vec<_>>()
        );
        audit["reference"] = match &reference {
            Some((chosen, _, pick)) => serde_json::json!({
                "source": chosen.source,
                "from": if pick.is_some() { "gathered" } else { "title" },
            }),
            None => serde_json::Value::Null,
        };
        let path = audit_path(self.cache_dir, youtube_id);
        if let Err(e) = tokio::fs::write(&path, audit.to_string()).await {
            warn!(path = %path.display(), %e, "title search: audit write failed");
        }

        let Some((i, _)) = found else {
            info!(%youtube_id, title, candidates = cands.len(), "title search: no usable lyric reaches the floor");
            return None;
        };
        let (chosen, lyric_score, pick) = reference?;
        info!(
            %youtube_id,
            title,
            provider = cands[i].provider.source(),
            id = %cands[i].id,
            artist = %cands[i].artist,
            score = scores[i],
            cleaned_score = lyric_score,
            reference = %chosen.source,
            from = if pick.is_some() { "gathered" } else { "title" },
            "title search: found a lyric (#144)"
        );
        Some(chosen)
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
