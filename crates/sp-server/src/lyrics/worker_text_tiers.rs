//! `LyricsWorker` extension — the text tiers of one song, after its vocal is
//! isolated (#144).
//!
//! 1. ONE Gemini 3.5 Transcribe transcript of the isolated vocal
//!    (`transcribe_vocal`), reused by everything below and kept on disk for
//!    a no-penalty deferral re-pick (`transcript_cache`). Before #144 the
//!    reference stage transcribed after mtl, and the base tier transcribed the
//!    same vocal AGAIN whenever the gate failed.
//! 2. The title search (`title_search`), when no artist+title lookup found the
//!    song (a cover names the cover artist) and the ★ tier can run:
//!    candidates by title alone; the one the transcript's words match best
//!    above a measured floor becomes the reference text, unless the video's
//!    own captions / description match what is sung at least as well.
//! 3. The ★ tier (`run_mtl_reference_stage`): the best text candidate, mtl
//!    force-aligned and verified by the two-way gate against that transcript.
//! 4. The base tier (`run_g35t_transcript_branch`): the transcript grouped
//!    into lines, for every song the ★ tier did not ship.
//!
//! Extracted from `worker.rs::process_song` to keep that file under the
//! 1000-line CI cap.

use std::path::Path;

use sp_core::lyrics::LyricsTrack;
use tracing::{info, warn};

use super::worker::LyricsWorker;
use crate::lyrics::g35t_client::AsrWord;
use crate::lyrics::orchestrator::ReferenceStageBackend;
use crate::lyrics::worker_outcome::SongOutcome;

/// What the text tiers hand back to `process_song`.
pub(crate) enum TierOutcome {
    /// An un-translated line-level track — the caller translates + persists it.
    Track(LyricsTrack),
    /// The song ends here for this pick (deferred, waiting or quarantined);
    /// the in-flight marker is already handled.
    Return(SongOutcome),
}

/// #144: whether a tier outcome ends the processing pass — a track (★ or
/// base tier), or the song done for this pass (a quarantine). Its kept
/// transcript is then retired, so the next pass transcribes afresh. A
/// deferral (penalized or not) does not end it: a re-pick may reuse it.
pub(crate) fn ends_the_pass(outcome: &TierOutcome) -> bool {
    matches!(
        outcome,
        TierOutcome::Track(_) | TierOutcome::Return(SongOutcome::Done)
    )
}

/// #144: whether the title search runs: the song has a non-empty transcript
/// to score against, no artist+title lookup found it
/// (`title_search::needs_title_search`), and the ★ tier can use a found
/// lyric (the mtl tooling is present).
pub(crate) fn should_title_search(
    words: &[AsrWord],
    candidates: &[crate::lyrics::tier1::CandidateText],
    mtl_available: bool,
) -> bool {
    !words.is_empty()
        && mtl_available
        && crate::lyrics::title_search::needs_title_search(candidates)
}

/// #144: the one g35t transcript of this song's isolated vocal.
///
/// A transcript kept for the same vocal by this processing pass
/// (`transcript_cache`) is reused — a no-penalty deferral re-pick does not
/// pay for it twice; a new one is kept, and `run_text_tiers` retires it when
/// the pass ends, so the next pass transcribes afresh. `Ok(None)` means there is nothing to
/// transcribe here: no isolated vocal (the base tier chooses between the #171
/// full-mix fallback and a deferral) or no Gemini key (the base tier defers
/// `gemini_key_missing`). `Err` is a failed transcription: the song defers
/// (`g35t_error`) and retries after its backoff — before #144 it spent the
/// mtl run first, then failed again in the base tier.
pub(crate) async fn transcribe_vocal(
    backend: &dyn ReferenceStageBackend,
    clean_vocal: Option<&Path>,
    gemini_keys: &[String],
    youtube_id: &str,
    cache_dir: &Path,
) -> Result<Option<Vec<AsrWord>>, &'static str> {
    use crate::lyrics::transcript_cache;

    let Some(wav) = clean_vocal else {
        return Ok(None);
    };
    let kept = transcript_cache::path(cache_dir, youtube_id);
    let vocal = tokio::fs::metadata(wav)
        .await
        .ok()
        .and_then(|m| transcript_cache::vocal_identity(&m));
    if let Some(vocal) = vocal
        && let Some(cached) = transcript_cache::load(&kept).await
        && transcript_cache::reusable(&cached, vocal, transcript_cache::now_ms())
    {
        info!(
            youtube_id = %youtube_id,
            words = cached.words.len(),
            "g35t: the song's transcript, kept from this vocal's earlier pick (#144)"
        );
        return Ok(Some(cached.words()));
    }
    if gemini_keys.is_empty() {
        return Ok(None);
    }
    match backend.asr_transcribe(wav).await {
        Ok(words) => {
            info!(
                youtube_id = %youtube_id,
                words = words.len(),
                "g35t: the song's one transcript (#144)"
            );
            if let Some(vocal) = vocal {
                transcript_cache::store(&kept, vocal, transcript_cache::now_ms(), &words).await;
            }
            Ok(Some(words))
        }
        Err(e) => {
            warn!(
                youtube_id = %youtube_id,
                error = %e,
                "g35t: transcription failed — deferring the song for retry"
            );
            Err("g35t_error")
        }
    }
}

impl LyricsWorker {
    /// The text tiers for one song (module doc). `candidate_texts` are what
    /// `gather_sources` found; `clean_vocal` is the isolated vocal (`None`
    /// when isolation yielded none). When the outcome ends the pass
    /// (`ends_the_pass`), the pass's kept transcript is retired — the ONE
    /// place that does it, for every ending.
    #[cfg_attr(test, mutants::skip)] // I/O glue; `ends_the_pass` and each step are tested
    pub(crate) async fn run_text_tiers(
        &self,
        row: &crate::db::models::VideoLyricsRow,
        candidate_texts: Vec<crate::lyrics::provider::CandidateText>,
        clean_vocal: Option<&Path>,
        gpu_mem: Option<String>,
        mode: crate::lyrics::heavy_plan::ProcessingMode,
        started_at_unix_ms: i64,
    ) -> anyhow::Result<TierOutcome> {
        let outcome = self
            .run_tiers(
                row,
                candidate_texts,
                clean_vocal,
                gpu_mem,
                mode,
                started_at_unix_ms,
            )
            .await?;
        if ends_the_pass(&outcome) {
            crate::lyrics::transcript_cache::retire(&self.cache_dir, &row.youtube_id).await;
        }
        Ok(outcome)
    }

    /// The tiers themselves (`run_text_tiers` retires the transcript after).
    #[cfg_attr(test, mutants::skip)] // I/O glue; each step is tested on its own
    async fn run_tiers(
        &self,
        row: &crate::db::models::VideoLyricsRow,
        candidate_texts: Vec<crate::lyrics::provider::CandidateText>,
        clean_vocal: Option<&Path>,
        gpu_mem: Option<String>,
        mode: crate::lyrics::heavy_plan::ProcessingMode,
        started_at_unix_ms: i64,
    ) -> anyhow::Result<TierOutcome> {
        let video_id = row.id;
        let youtube_id = row.youtube_id.as_str();
        let candidates: Vec<crate::lyrics::tier1::CandidateText> = candidate_texts
            .into_iter()
            .map(crate::lyrics::tier1::CandidateText::from)
            .collect();

        // The Gemini key list, parsed once for the transcript and the base tier.
        let gemini_csv = crate::db::models::get_setting(&self.pool, "gemini_api_key")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let gemini_keys = crate::gemini_api::gemini_keys_from_setting(&gemini_csv);

        let reference_backend = crate::lyrics::orchestrator::RealReferenceStageBackend {
            mtl_cfg: crate::lyrics::mtl_aligner::MtlConfig::from_tools_dir(&self.tools_dir),
            work_dir: self.cache_dir.clone(),
            http_client: self.client.clone(),
            gemini_keys: gemini_keys.clone(),
            gpu_mem_setting: gpu_mem,
            // #161/#162: the backend wraps ONLY the mtl subprocess (never the
            // g35t HTTP transcription), using these handles + the once-read
            // processing mode (which selects the mtl plan + abort arming).
            ndi_health_registry: self.ndi_health_registry.clone(),
            obs_state: self.obs_state.clone(),
            mode,
        };

        let transcript = match transcribe_vocal(
            &reference_backend,
            clean_vocal,
            &gemini_keys,
            youtube_id,
            &self.cache_dir,
        )
        .await
        {
            Ok(t) => t,
            Err(reason) => {
                // `process_next` records the backoff (`defer_song`).
                self.clear_processing().await;
                return Ok(TierOutcome::Return(SongOutcome::Deferred(reason)));
            }
        };

        // #144: a cover's text by title, chosen by what is sung
        // (`should_title_search`). Unlike the transcript it is not kept for
        // the pass: a no-penalty deferral re-pick searches again (the Claude
        // cleanup of a chosen plain lyric is cached; the LRCLIB / Genius
        // answers may differ, and the gate verifies whatever is chosen).
        let words = transcript.as_deref().unwrap_or_default();
        let title_reference =
            if should_title_search(words, &candidates, reference_backend.mtl_cfg.is_available()) {
                self.title_search_reference(row, words, &candidates).await
            } else {
                crate::lyrics::title_search::remove_audit(&self.cache_dir, youtube_id).await;
                None
            };

        // Tier 1 (★): the reference text — the title search's choice, else the
        // best gathered candidate by source priority — mtl force-aligned and
        // verified by the two-way gate against the transcript. On PASS the mtl
        // line timings ship; otherwise (skip / gate fail / mtl error) the base
        // tier below.
        let best_candidate = title_reference.or_else(|| {
            crate::lyrics::claude_merge::best_authoritative_candidate(&candidates).cloned()
        });

        // #154 gate #2 (idle-only mode only, #162). Isolation above may have
        // started while the wall was idle and finished after it went busy — a
        // running subprocess is never killed (that would waste ~4 min of GPU
        // work), so the check goes here, BEFORE the next heavy spawn (mtl). If
        // the wall is busy now, defer the WHOLE song (WaitingForWall): the
        // isolated vocal WAV and the transcript are kept on disk, so the next
        // idle pick is a cache-hit isolation + mtl (only the title search, if
        // it runs, asks again). No mtl runs on an empty transcript, so nothing
        // defers for one. We do NOT fall
        // through to the g35t base tier — that would degrade the ★ mtl tier
        // (owner's quality-first rule). In LOW-PRIORITY mode there is no gate #2:
        // mtl runs at reduced priority instead (the backend picks the plan and
        // re-runs on CPU if a GPU job is aborted).
        if mode == crate::lyrics::heavy_plan::ProcessingMode::IdleOnly
            && best_candidate.is_some()
            && clean_vocal.is_some()
            && !words.is_empty()
            && self.defer_before_mtl().await
        {
            return Ok(TierOutcome::Return(SongOutcome::WaitingForWall));
        }

        let mtl_track = match self
            .run_mtl_reference_stage(
                video_id,
                youtube_id,
                best_candidate.as_ref(),
                clean_vocal,
                words,
                &reference_backend,
            )
            .await
        {
            Ok(t) => t,
            // #161 wall-abort / #162 low memory during/before mtl → defer the
            // whole song with NO penalty (defer_heavy). Never fall through to the
            // g35t base tier (that would degrade the ★ mtl tier, owner's
            // quality-first rule); the next pick re-runs mtl to byte-identical ★.
            Err(d) => return Ok(TierOutcome::Return(self.defer_heavy(d).await)),
        };
        if let Some(track) = mtl_track {
            return Ok(TierOutcome::Track(track));
        }

        // Tier 2 — the g35t base tier: the transcript grouped into lines.
        match self
            .run_g35t_transcript_branch(
                clean_vocal,
                // #171: mix FLAC — base-tier last resort when isolation never yields a vocal.
                row.audio_file_path.as_deref().map(Path::new),
                transcript,
                &gemini_keys,
                video_id,
                youtube_id,
                &row.song,
                &row.artist,
                started_at_unix_ms,
            )
            .await?
        {
            crate::lyrics::worker_g35t::G35tOutcome::Track(track) => Ok(TierOutcome::Track(track)),
            crate::lyrics::worker_g35t::G35tOutcome::Deferred(reason) => {
                // Vocals WAV intentionally preserved on disk — aligner's
                // cache-hit path reuses it on the next run.
                self.clear_processing().await;
                Ok(TierOutcome::Return(SongOutcome::Deferred(reason)))
            }
            crate::lyrics::worker_g35t::G35tOutcome::Quarantined => {
                self.clear_processing().await;
                Ok(TierOutcome::Return(SongOutcome::Done))
            }
        }
    }

    /// #144: the title search for this song (`title_search::TitleSearch`), on
    /// the production endpoints: the reference text when it found a lyric.
    /// The Genius token is read per song, like the artist+title Genius lookup
    /// (`gather_sources`); empty = LRCLIB only.
    #[cfg_attr(test, mutants::skip)] // settings read + wiring; `TitleSearch::find` is tested
    async fn title_search_reference(
        &self,
        row: &crate::db::models::VideoLyricsRow,
        words: &[AsrWord],
        gathered: &[crate::lyrics::tier1::CandidateText],
    ) -> Option<crate::lyrics::tier1::CandidateText> {
        let genius_token = crate::db::models::get_setting(&self.pool, "genius_access_token")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let search = crate::lyrics::title_search::TitleSearch {
            client: &self.client,
            ai: self.ai_client.as_deref(),
            genius_token: &genius_token,
            cache_dir: &self.cache_dir,
        };
        search
            .find(
                &crate::lyrics::title_search::TitleSearchEndpoints::production(),
                row,
                words,
                gathered,
            )
            .await
    }
}

#[cfg(test)]
#[path = "worker_text_tiers_tests.rs"]
mod tests;
