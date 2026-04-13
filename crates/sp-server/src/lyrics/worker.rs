//! Lyrics worker orchestrator.
//!
//! Processes one video at a time through the full lyrics pipeline:
//! LRCLIB → YouTube subs → Qwen3-ASR → forced alignment → Gemini translation → persist.

use anyhow::Result;
use reqwest::Client;
use sp_core::lyrics::LyricsTrack;
use sqlx::SqlitePool;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::{
    db::models::{
        get_next_video_missing_translation, get_next_video_without_lyrics, mark_video_lyrics,
    },
    lyrics::{aligner, lrclib, translator},
};

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

#[allow(dead_code)]
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

    /// Write the embedded Python helper script to disk so the subprocess can
    /// find it. Overwrites on every startup to keep the script in sync with
    /// the Rust binary version.
    #[cfg_attr(test, mutants::skip)]
    async fn ensure_script(&self) -> Result<()> {
        if let Some(parent) = self.script_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(
            &self.script_path,
            include_str!("../../../../scripts/lyrics_worker.py"),
        )
        .await?;
        tracing::info!("lyrics_worker: wrote {}", self.script_path.display());
        Ok(())
    }

    // ---------------------------------------------------------------------------
    // Main loop
    // ---------------------------------------------------------------------------

    #[cfg_attr(test, mutants::skip)]
    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        tracing::info!("lyrics_worker: started");

        // Ensure the Python helper script is written to disk.
        if let Err(e) = self.ensure_script().await {
            error!("lyrics_worker: failed to write lyrics_worker.py: {e}");
        }
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
                // No new songs — try retranslating songs missing SK
                self.retry_missing_translations().await;
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
                debug!("lyrics_worker: no lyrics for {youtube_id}: {e}");
                if let Err(db_err) =
                    mark_video_lyrics(&self.pool, video_id, false, Some("no_source")).await
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

        // Step 2: Forced alignment — DISABLED until Qwen3 model compatibility
        // is resolved with transformers library. The model architecture
        // "qwen3_asr" is not recognized by transformers 5.5.3.
        // See: https://github.com/QwenLM/Qwen3-ASR
        if false {
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
                    debug!(
                        "lyrics_worker: no audio_file_path for {youtube_id}, skipping alignment"
                    );
                }
            }
        }

        // Step 3: Gemini translation (if API key non-empty)
        if !self.gemini_api_key.is_empty() {
            match translator::translate_lyrics(&self.gemini_api_key, &self.gemini_model, &mut track)
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

        // 2. YouTube subs — DISABLED: auto-generated subs are unusable for
        // karaoke (full of [music] markers, overlapping text, garbled phrases).
        // Will be re-enabled when Gemini-based lyrics extraction is implemented
        // or Qwen3-ASR alignment (#25) is working.

        // 3. Qwen3-ASR — DISABLED until model compatibility resolved (#25)

        // No usable source found — mark as no_source so it's skipped
        anyhow::bail!("no usable lyrics source for {youtube_id}");
    }

    /// Retry Gemini translation for songs that have lyrics but no SK.
    #[cfg_attr(test, mutants::skip)]
    async fn retry_missing_translations(&self) {
        if self.gemini_api_key.is_empty() {
            return;
        }
        let result = get_next_video_missing_translation(&self.pool, &self.cache_dir).await;
        let (_video_id, youtube_id) = match result {
            Ok(Some(pair)) => pair,
            _ => return,
        };

        let lyrics_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let content = match tokio::fs::read_to_string(&lyrics_path).await {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut track: LyricsTrack = match serde_json::from_str(&content) {
            Ok(t) => t,
            Err(_) => return,
        };

        info!("lyrics_worker: retrying translation for {youtube_id}");
        match translator::translate_lyrics(&self.gemini_api_key, &self.gemini_model, &mut track)
            .await
        {
            Ok(()) => {
                let json = serde_json::to_vec(&track).unwrap_or_default();
                let _ = tokio::fs::write(&lyrics_path, &json).await;
                info!("lyrics_worker: translation retry succeeded for {youtube_id}");
            }
            Err(e) => {
                debug!("lyrics_worker: translation retry failed for {youtube_id}: {e}");
            }
        }
    }
}
