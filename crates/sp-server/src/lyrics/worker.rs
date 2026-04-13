//! Lyrics worker orchestrator.
//!
//! Processes one video at a time through the full lyrics pipeline:
//! LRCLIB → YouTube subs → Qwen3-ASR → forced alignment → Gemini translation → persist.

use anyhow::Result;
use reqwest::Client;
use sp_core::lyrics::{LyricsLine, LyricsTrack};
use sqlx::SqlitePool;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing::{debug, error, warn};

use crate::{
    db::models::{get_next_video_without_lyrics, mark_video_lyrics},
    lyrics::{aligner, lrclib, translator, youtube_subs},
};

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

pub struct LyricsWorker {
    pool: SqlitePool,
    client: Client,
    cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    python_path: Option<PathBuf>,
    script_path: PathBuf,
    models_dir: PathBuf,
    gemini_api_key: String,
    gemini_model: String,
}

impl LyricsWorker {
    /// Create a new `LyricsWorker`.
    ///
    /// `tools_dir` is the directory containing `lyrics_worker.py`; the HuggingFace
    /// model cache is placed in `{tools_dir}/hf_models`.
    pub fn new(
        pool: SqlitePool,
        cache_dir: PathBuf,
        ytdlp_path: PathBuf,
        python_path: Option<PathBuf>,
        tools_dir: PathBuf,
        gemini_api_key: String,
        gemini_model: String,
    ) -> Self {
        let script_path = tools_dir.join("lyrics_worker.py");
        let models_dir = tools_dir.join("hf_models");
        Self {
            pool,
            client: Client::new(),
            cache_dir,
            ytdlp_path,
            python_path,
            script_path,
            models_dir,
            gemini_api_key,
            gemini_model,
        }
    }

    // ---------------------------------------------------------------------------
    // Main loop
    // ---------------------------------------------------------------------------

    #[cfg_attr(test, mutants::skip)]
    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        tracing::info!("lyrics_worker: started");
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => { break; }
                _ = self.process_next() => {}
            }
            // 5-second throttle between songs
            tokio::select! {
                _ = shutdown_rx.recv() => { break; }
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            }
        }
        tracing::info!("lyrics_worker: stopped");
    }

    // ---------------------------------------------------------------------------
    // process_next
    // ---------------------------------------------------------------------------

    #[cfg_attr(test, mutants::skip)]
    async fn process_next(&self) {
        let row = match get_next_video_without_lyrics(&self.pool).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                debug!("lyrics_worker: no pending videos");
                return;
            }
            Err(e) => {
                error!("lyrics_worker: DB query failed: {e}");
                return;
            }
        };

        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();

        tracing::info!(
            "lyrics_worker: processing video {} ({} - {})",
            youtube_id,
            row.artist,
            row.song
        );

        match self.process_song(row).await {
            Ok(()) => {}
            Err(e) => {
                error!("lyrics_worker: failed to process video {youtube_id}: {e}");
                // Mark as failed (has_lyrics=0 but with a source indicating failure)
                if let Err(db_err) =
                    mark_video_lyrics(&self.pool, video_id, false, Some("failed")).await
                {
                    error!("lyrics_worker: failed to mark video {youtube_id} as failed: {db_err}");
                }
            }
        }
    }

    // ---------------------------------------------------------------------------
    // process_song — main pipeline
    // ---------------------------------------------------------------------------

    #[cfg_attr(test, mutants::skip)]
    async fn process_song(&self, row: crate::db::models::VideoLyricsRow) -> Result<()> {
        let video_id = row.id;
        let youtube_id = &row.youtube_id;

        // Step 1: Acquire lyrics via source waterfall
        let (mut track, mut source) = self.acquire_lyrics(&row).await?;

        // Step 2: Forced alignment (if python available and audio file exists)
        if let Some(python) = &self.python_path {
            if let Some(audio_path) = &row.audio_file_path {
                let audio = PathBuf::from(audio_path);
                if audio.exists() {
                    let lyrics_text: String = track
                        .lines
                        .iter()
                        .map(|l| l.en.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");

                    let output_path = self
                        .cache_dir
                        .join(format!("{youtube_id}_align_output.json"));

                    match aligner::align_lyrics(
                        python,
                        &self.script_path,
                        &self.models_dir,
                        &audio,
                        &lyrics_text,
                        &output_path,
                    )
                    .await
                    {
                        Ok(aligned_lines) => {
                            track.lines = aligned_lines;
                            source = format!("{source}+aligner");
                            track.source = source.clone();
                        }
                        Err(e) => {
                            warn!(
                                "lyrics_worker: alignment failed for {youtube_id}, keeping original timestamps: {e}"
                            );
                        }
                    }
                } else {
                    debug!(
                        "lyrics_worker: audio file not found for {youtube_id}, skipping alignment"
                    );
                }
            } else {
                debug!("lyrics_worker: no audio_file_path for {youtube_id}, skipping alignment");
            }
        }

        // Step 3: Gemini translation (if API key non-empty)
        if !self.gemini_api_key.is_empty() {
            match translator::translate_lyrics(
                &self.client,
                &self.gemini_api_key,
                &self.gemini_model,
                &mut track,
            )
            .await
            {
                Ok(()) => {
                    debug!("lyrics_worker: translation succeeded for {youtube_id}");
                }
                Err(e) => {
                    warn!(
                        "lyrics_worker: translation failed for {youtube_id}, persisting EN only: {e}"
                    );
                }
            }
        }

        // Step 4: Persist
        let json_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let json_bytes = serde_json::to_vec(&track)?;
        tokio::fs::write(&json_path, &json_bytes).await?;

        mark_video_lyrics(&self.pool, video_id, true, Some(&source)).await?;

        tracing::info!("lyrics_worker: persisted lyrics for {youtube_id} (source={source})");

        Ok(())
    }

    // ---------------------------------------------------------------------------
    // acquire_lyrics — source waterfall
    // ---------------------------------------------------------------------------

    #[cfg_attr(test, mutants::skip)]
    async fn acquire_lyrics(
        &self,
        row: &crate::db::models::VideoLyricsRow,
    ) -> Result<(LyricsTrack, String)> {
        let youtube_id = &row.youtube_id;

        // 1. LRCLIB (if song/artist non-empty)
        if !row.song.is_empty() && !row.artist.is_empty() {
            let duration_s = row.duration_ms.map(|ms| (ms / 1000) as u32).unwrap_or(0);

            match lrclib::fetch_lyrics(&self.client, &row.artist, &row.song, duration_s).await {
                Ok(Some(track)) => {
                    debug!("lyrics_worker: LRCLIB hit for {youtube_id}");
                    return Ok((track, "lrclib".to_string()));
                }
                Ok(None) => {
                    debug!("lyrics_worker: LRCLIB miss for {youtube_id}");
                }
                Err(e) => {
                    warn!("lyrics_worker: LRCLIB error for {youtube_id}: {e}");
                }
            }
        }

        // 2. YouTube subs
        let subs_temp_dir = self.cache_dir.join("_subs_temp");
        let _ = tokio::fs::create_dir_all(&subs_temp_dir).await;

        let subs_result =
            youtube_subs::fetch_subtitles(&self.ytdlp_path, youtube_id, &subs_temp_dir).await;

        // Clean up temp dir regardless of outcome
        let _ = tokio::fs::remove_dir_all(&subs_temp_dir).await;

        match subs_result {
            Ok(Some(track)) => {
                debug!("lyrics_worker: YouTube subs hit for {youtube_id}");
                return Ok((track, "youtube".to_string()));
            }
            Ok(None) => {
                debug!("lyrics_worker: YouTube subs miss for {youtube_id}");
            }
            Err(e) => {
                warn!("lyrics_worker: YouTube subs error for {youtube_id}: {e}");
            }
        }

        // 3. Qwen3-ASR (if python available and audio file exists)
        if let Some(python) = &self.python_path {
            if let Some(audio_path) = &row.audio_file_path {
                let audio = PathBuf::from(audio_path);
                if audio.exists() {
                    let output_path = self.cache_dir.join(format!("{youtube_id}_asr_output.json"));

                    match aligner::transcribe_audio(
                        python,
                        &self.script_path,
                        &self.models_dir,
                        &audio,
                        &output_path,
                    )
                    .await
                    {
                        Ok(text) if !text.trim().is_empty() => {
                            debug!("lyrics_worker: ASR transcription succeeded for {youtube_id}");

                            let lines: Vec<LyricsLine> = text
                                .lines()
                                .map(str::trim)
                                .filter(|l| !l.is_empty())
                                .map(|l| LyricsLine {
                                    start_ms: 0,
                                    end_ms: 0,
                                    en: l.to_string(),
                                    sk: None,
                                    words: None,
                                })
                                .collect();

                            if !lines.is_empty() {
                                let track = LyricsTrack {
                                    version: 1,
                                    source: "asr".to_string(),
                                    language_source: "en".to_string(),
                                    language_translation: String::new(),
                                    lines,
                                };
                                return Ok((track, "asr".to_string()));
                            }
                        }
                        Ok(_) => {
                            debug!("lyrics_worker: ASR returned empty text for {youtube_id}");
                        }
                        Err(e) => {
                            warn!("lyrics_worker: ASR failed for {youtube_id}: {e}");
                        }
                    }
                } else {
                    debug!("lyrics_worker: audio file not found for ASR for {youtube_id}");
                }
            }
        }

        // All sources failed
        anyhow::bail!("all lyrics sources failed for video {youtube_id}");
    }
}
