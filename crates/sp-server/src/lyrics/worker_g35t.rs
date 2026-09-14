//! `LyricsWorker` extension — v22 (#159) g35t base tier.
//!
//! The single base-tier route for songs the v21 mtl reference stage does not
//! ship (no usable text candidate, gate fail, or mtl skip/error). Replaces the
//! deleted WhisperX (`Orchestrator`) fallback and the AssemblyAI `asr_path`
//! branch with one Gemini-only path: transcribe the isolated vocals with
//! Gemini 3.5 Transcribe (`g35t_client`), group the words into LED-wall lines
//! (`g35t_transcript`), and hand the un-translated `LyricsTrack` back to the
//! worker's shared translate+persist tail — exactly like the mtl ★ path.
//!
//! Vocal isolation is NOT done here — `process_song` already isolated once (for
//! the reference stage) and passes the same `clean_vocal` in, so there is no
//! duplicate Demucs pass. Extracted from `worker.rs::process_song` to keep that
//! file under the 1000-line CI limit.

use std::path::Path;

use anyhow::Result;
use sp_core::lyrics::LyricsTrack;
use tracing::warn;

use super::worker::LyricsWorker;
use crate::lyrics::LYRICS_PIPELINE_VERSION;

/// Outcome of the g35t base tier. `Track` flows to the worker's shared
/// translate+persist tail; `Deferred`/`Quarantined` are terminal for this song.
pub(crate) enum G35tOutcome {
    /// Un-translated line-level track — the caller translates + persists it.
    Track(LyricsTrack),
    /// Transient failure (no vocals / no keys / g35t transport error) — the
    /// selector backs the row off and retries later.
    Deferred(&'static str),
    /// No usable transcript — quarantined as `asr_gap` (already written here).
    Quarantined,
}

impl LyricsWorker {
    /// Run the Gemini 3.5 Transcribe base tier for a song the reference stage
    /// did not ship. `clean_vocal` is the already-isolated vocal stem from
    /// `process_song`; `gemini_keys` is the parsed `gemini_api_key` CSV.
    ///
    /// Does NOT translate/persist or call `clear_processing` — the caller does
    /// that (Track → shared tail; Deferred/Quarantined → early return).
    #[cfg_attr(test, mutants::skip)]
    // network + DB glue; the pure grouping/
    // build decision is unit-tested in `g35t_transcript::build_track`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_g35t_transcript_branch(
        &self,
        clean_vocal: Option<&Path>,
        gemini_keys: &[String],
        video_id: i64,
        youtube_id: &str,
        song: &str,
        artist: &str,
        started_at_unix_ms: i64,
    ) -> Result<G35tOutcome> {
        let Some(wav) = clean_vocal else {
            warn!(
                youtube_id = %youtube_id,
                "g35t base tier: no preprocessed vocal — leaving row unprocessed"
            );
            return Ok(G35tOutcome::Deferred("vocal_isolation_failed"));
        };
        if gemini_keys.is_empty() {
            warn!(
                youtube_id = %youtube_id,
                "g35t base tier: gemini_api_key not set — leaving row unprocessed"
            );
            return Ok(G35tOutcome::Deferred("gemini_key_missing"));
        }

        self.broadcast_stage(
            video_id,
            youtube_id,
            song,
            artist,
            "aligning",
            None,
            started_at_unix_ms,
        )
        .await;

        let words = match crate::lyrics::g35t_client::transcribe_words(
            &self.client,
            gemini_keys,
            wav,
            &["en-US".to_string()],
        )
        .await
        {
            Ok(w) => w,
            Err(e) => {
                warn!(
                    youtube_id = %youtube_id,
                    error = %e,
                    "g35t base tier: transcription failed — leaving row unprocessed for retry"
                );
                return Ok(G35tOutcome::Deferred("g35t_error"));
            }
        };

        // Audit sidecar: dump the word count so operators can inspect coverage
        // without parsing tracing logs (mirrors the old asr_path audit).
        let audit_path = self.cache_dir.join(format!("{youtube_id}_g35t_audit.json"));
        let audit = serde_json::json!({
            "outcome": if words.is_empty() { "quarantine" } else { "lines" },
            "g35t_word_count": words.len(),
        });
        if let Ok(bytes) = serde_json::to_vec_pretty(&audit) {
            let _ = tokio::fs::write(&audit_path, &bytes).await;
        }

        match crate::lyrics::g35t_transcript::build_track(&words, LYRICS_PIPELINE_VERSION) {
            Some(track) => {
                tracing::info!(
                    youtube_id = %youtube_id,
                    source = %track.source,
                    lines = track.lines.len(),
                    "g35t base tier: transcript grouped into lines"
                );
                Ok(G35tOutcome::Track(track))
            }
            None => {
                warn!(
                    youtube_id = %youtube_id,
                    "g35t base tier: empty transcript — quarantining as asr_gap"
                );
                if let Err(e) = crate::db::models::quarantine_video_lyrics(
                    &self.pool,
                    video_id,
                    &self.cache_dir,
                    "empty_transcript",
                    LYRICS_PIPELINE_VERSION,
                )
                .await
                {
                    warn!(youtube_id = %youtube_id, %e, "g35t base tier: quarantine failed");
                }
                Ok(G35tOutcome::Quarantined)
            }
        }
    }
}
