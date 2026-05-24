//! `LyricsWorker` extension — asr_path branch.
//!
//! Extracted from `worker.rs::process_song` to keep that file under the
//! 1000-line CI limit. The branch runs when whisperx's gate rejects the song
//! but a text candidate (genius / lrclib-untimed) still exists. Uses AAI
//! transcription + silence-gap split + a drop-safe index-level Claude regroup
//! (see `asr_path/mod.rs`). See
//! `docs/superpowers/specs/2026-05-19-asr-path-aai-claude-merge-design.md`
//! (the word-index Claude-merge in that spec is superseded).

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use sp_core::lyrics::LyricsTrack;
use sp_core::ws::ServerMsg;
use tracing::warn;

use super::worker::LyricsWorker;
use crate::lyrics::LYRICS_PIPELINE_VERSION;

impl LyricsWorker {
    /// Run the AssemblyAI Universal-3 Pro alignment path for a song the
    /// whisperx gate rejected. AAI transcribes the vocal, then the
    /// silence-gap splitter groups words into singable lines (no Claude-merge).
    ///
    /// `candidate_texts` (genius / lrclib lines) are passed to AAI as
    /// `keyterms_prompt` — biasing recognition toward the real lyrics so the
    /// model resolves sung words correctly instead of guessing. The text is a
    /// HELPER input only; it never adds or drops lines.
    ///
    /// Does NOT call `self.clear_processing()` — the caller does that
    /// immediately after the await so there is exactly one call site.
    #[cfg_attr(test, mutants::skip)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_asr_path_branch(
        &self,
        candidate_texts: &[crate::lyrics::provider::CandidateText],
        audio_file_path: Option<&str>,
        video_id: i64,
        youtube_id: &str,
        song: &str,
        artist: &str,
        started_at_unix_ms: i64,
        start_instant: Instant,
    ) -> Result<()> {
        let aai_key = crate::db::models::get_setting(
            &self.pool,
            crate::lyrics::asr_path::ASSEMBLYAI_API_KEY_SETTING,
        )
        .await
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty());

        let aai_key = match aai_key {
            Some(k) => k,
            None => {
                warn!(
                    youtube_id = %youtube_id,
                    "asr_path: assemblyai_api_key not set — leaving row unprocessed"
                );
                return Ok(());
            }
        };

        self.broadcast_stage(
            video_id,
            youtube_id,
            song,
            artist,
            "preprocessing",
            None,
            started_at_unix_ms,
        )
        .await;

        // Vocal isolation — reuse existing preprocess_vocals.
        let venv_python = self.venv_python.read().await.clone();
        let audio_path: Option<PathBuf> = audio_file_path.map(PathBuf::from);
        let clean_vocal: Option<PathBuf> = match (&venv_python, &audio_path) {
            (Some(python), Some(audio)) if audio.exists() => {
                let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
                match crate::lyrics::aligner::preprocess_vocals(
                    python,
                    &self.script_path,
                    &self.models_dir,
                    audio,
                    &wav_path,
                )
                .await
                {
                    Ok(p) => Some(p),
                    Err(e) => {
                        warn!(
                            youtube_id = %youtube_id,
                            error = %e,
                            "asr_path: vocal isolation failed"
                        );
                        None
                    }
                }
            }
            _ => None,
        };

        let Some(wav) = clean_vocal else {
            warn!(
                youtube_id = %youtube_id,
                "asr_path: no preprocessed vocal available — leaving row unprocessed"
            );
            return Ok(());
        };

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

        // Build keyterms from every gathered reference line (genius/lrclib/…)
        // to bias AAI recognition. Dedup, drop blanks, cap to AAI's limit.
        let mut keyterms: Vec<String> = candidate_texts
            .iter()
            .flat_map(|c| c.lines.iter())
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        keyterms.sort();
        keyterms.dedup();
        keyterms.truncate(1000);

        let aai = crate::lyrics::asr_path::aai_backend::AaiBackend::new(aai_key);
        let ai_client = self.ai_client.clone();
        let chat_ref: Option<&dyn crate::lyrics::asr_path::regroup::RegroupChat> =
            match ai_client.as_ref() {
                Some(c) => Some(c.as_ref() as &dyn crate::lyrics::asr_path::regroup::RegroupChat),
                None => None,
            };
        let result = crate::lyrics::asr_path::run(&aai, chat_ref, &wav, &keyterms, &keyterms).await;

        // Write audit sidecar regardless of outcome — operators can grep these
        // to understand what happened on each row without parsing tracing logs.
        if let Ok(ref r) = result {
            let audit_path = self
                .cache_dir
                .join(format!("{youtube_id}_asr_path_audit.json"));
            if let Ok(bytes) = serde_json::to_vec_pretty(&r.audit) {
                let _ = tokio::fs::write(&audit_path, &bytes).await;
            }
        }

        match result {
            Ok(crate::lyrics::asr_path::AsrResult {
                output: crate::lyrics::asr_path::AsrOutput::Lines { lines, source },
                ..
            }) => {
                let mut track = LyricsTrack {
                    version: LYRICS_PIPELINE_VERSION,
                    source: source.to_string(),
                    language_source: "en".into(),
                    language_translation: String::new(),
                    lines,
                };

                self.broadcast_stage(
                    video_id,
                    youtube_id,
                    song,
                    artist,
                    "translating",
                    None,
                    started_at_unix_ms,
                )
                .await;
                self.translate_track(&mut track, youtube_id).await;

                self.broadcast_stage(
                    video_id,
                    youtube_id,
                    song,
                    artist,
                    "persisting",
                    None,
                    started_at_unix_ms,
                )
                .await;

                let json_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
                let json_bytes = serde_json::to_vec(&track)?;
                tokio::fs::write(&json_path, &json_bytes).await?;

                if let Err(e) = crate::db::models::mark_video_lyrics_complete(
                    &self.pool,
                    video_id,
                    &track.source,
                    LYRICS_PIPELINE_VERSION,
                    None,
                    Some(crate::lyrics::ALIGNMENT_MODEL_ASSEMBLYAI_U3_PRO_REV1),
                )
                .await
                {
                    warn!(
                        youtube_id = %youtube_id,
                        error = %e,
                        "asr_path: mark_video_lyrics_complete failed"
                    );
                }

                tracing::info!(
                    youtube_id = %youtube_id,
                    source = %track.source,
                    version = LYRICS_PIPELINE_VERSION,
                    "asr_path: persisted"
                );

                let duration_ms = start_instant.elapsed().as_millis() as u64;
                let _ = self.events_tx.send(ServerMsg::LyricsCompleted {
                    video_id,
                    youtube_id: youtube_id.to_string(),
                    source: track.source.clone(),
                    quality_score: 0.0,
                    provider_count: 1,
                    duration_ms,
                });
            }
            Ok(crate::lyrics::asr_path::AsrResult {
                output: crate::lyrics::asr_path::AsrOutput::Quarantine { reason },
                ..
            }) => {
                warn!(
                    youtube_id = %youtube_id,
                    reason,
                    "asr_path: quarantining as asr_gap"
                );
                if let Err(e) = crate::db::models::quarantine_video_lyrics(
                    &self.pool,
                    video_id,
                    &self.cache_dir,
                    reason,
                    LYRICS_PIPELINE_VERSION,
                )
                .await
                {
                    warn!("worker: quarantine_video_lyrics: {e}");
                }
            }
            Err(crate::lyrics::asr_path::AsrError::QuotaExhausted) => {
                warn!(
                    youtube_id = %youtube_id,
                    "asr_path: AAI quota exhausted — surface to operator"
                );
                // Emit a stage event so dashboard can highlight quota state.
                // The frontend displays "stage" string; "asr_quota_exhausted"
                // is a distinct label operators can grep for.
                let _ = self.events_tx.send(ServerMsg::LyricsProcessingStage {
                    video_id,
                    youtube_id: youtube_id.to_string(),
                    stage: "asr_quota_exhausted".to_string(),
                    provider: Some("assemblyai".to_string()),
                });
            }
            Err(e) => {
                warn!(
                    youtube_id = %youtube_id,
                    error = %e,
                    "asr_path: error — leaving row unprocessed for retry"
                );
            }
        }

        Ok(())
    }
}
