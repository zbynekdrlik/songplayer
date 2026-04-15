//! Lyrics worker orchestrator.
//!
//! Per-song decision tree:
//!   1. acquire_lyrics: YT manual subs first, then LRCLIB fallback, else bail.
//!   2. If source == "yt_subs": run chunked Qwen3 alignment.
//!   3. Gemini SK translation.
//!   4. Persist JSON + DB row.

use anyhow::Result;
use reqwest::Client;
use sp_core::lyrics::LyricsTrack;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::{
    db::models::{
        get_next_video_missing_translation, get_next_video_without_lyrics, mark_video_lyrics,
    },
    lyrics::{aligner, assembly, chunking, lrclib, quality, translator, youtube_subs},
};

const DUPLICATE_START_WARN_PCT: f64 = 50.0;

#[allow(dead_code)]
pub struct LyricsWorker {
    pool: SqlitePool,
    client: Client,
    cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    python_path: Option<PathBuf>,
    tools_dir: PathBuf,
    script_path: PathBuf,
    models_dir: PathBuf,
    gemini_api_key: String,
    gemini_model: String,
    venv_python: tokio::sync::RwLock<Option<PathBuf>>,
    retry_backoff: tokio::sync::Mutex<RetryBackoff>,
}

#[derive(Default)]
struct RetryBackoff {
    silent_until: Option<Instant>,
    consecutive_failures: u32,
}

impl LyricsWorker {
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
            tools_dir,
            script_path,
            models_dir,
            gemini_api_key,
            gemini_model,
            venv_python: tokio::sync::RwLock::new(None),
            retry_backoff: tokio::sync::Mutex::new(RetryBackoff::default()),
        }
    }

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

    #[cfg_attr(test, mutants::skip)]
    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        tracing::info!("lyrics_worker: started");

        if let Err(e) = self.ensure_script().await {
            error!("lyrics_worker: failed to write lyrics_worker.py: {e}");
        }

        if let Some(sys_py) = self.python_path.as_ref() {
            match crate::lyrics::bootstrap::ensure_ready(
                &self.tools_dir,
                &self.script_path,
                &self.models_dir,
                sys_py,
            )
            .await
            {
                Ok(Some(venv)) => {
                    tracing::info!("lyrics_worker: aligner ready at {}", venv.display());
                    *self.venv_python.write().await = Some(venv);
                }
                Ok(None) => tracing::info!("lyrics_worker: alignment disabled (non-Windows)"),
                Err(e) => warn!("lyrics_worker: bootstrap failed, alignment disabled: {e}"),
            }
        } else {
            warn!("lyrics_worker: no system Python, alignment disabled");
        }

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = self.process_next() => {}
            }
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
        tracing::info!("lyrics_worker: stopped");
    }

    #[cfg_attr(test, mutants::skip)]
    async fn process_next(&self) {
        let row = match get_next_video_without_lyrics(&self.pool).await {
            Ok(Some(r)) => r,
            Ok(None) => {
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

    #[cfg_attr(test, mutants::skip)]
    async fn process_song(&self, row: crate::db::models::VideoLyricsRow) -> Result<()> {
        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();

        // Step 1: Acquire lyrics. YT subs first, LRCLIB fallback.
        let (track, acquired_source) = self.acquire_lyrics(&row).await?;

        // Step 2: If the source is YT manual subs and a venv is ready, run
        // chunked alignment to populate word-level timestamps.
        let (mut track, final_source) = if acquired_source == "yt_subs" {
            let venv_python = self.venv_python.read().await.clone();
            let audio_path = row.audio_file_path.as_ref().map(PathBuf::from);
            if let (Some(python), Some(audio)) = (venv_python.as_ref(), audio_path.as_ref()) {
                if audio.exists() {
                    match self
                        .run_chunked_alignment(python, audio, &youtube_id, track)
                        .await
                    {
                        Ok(t) => (t, "yt_subs+qwen3".to_string()),
                        Err((original, e)) => {
                            warn!("lyrics_worker: chunked alignment failed for {youtube_id}: {e}");
                            (original, "yt_subs".to_string())
                        }
                    }
                } else {
                    debug!("lyrics_worker: alignment skipped for {youtube_id} (audio missing)");
                    (track, "yt_subs".to_string())
                }
            } else {
                debug!("lyrics_worker: alignment skipped for {youtube_id} (no venv or audio)");
                (track, "yt_subs".to_string())
            }
        } else {
            (track, acquired_source)
        };
        track.source = final_source.clone();

        // Step 3: Gemini translation (if configured).
        if !self.gemini_api_key.is_empty() {
            if let Err(e) =
                translator::translate_lyrics(&self.gemini_api_key, &self.gemini_model, &mut track)
                    .await
            {
                warn!(
                    "lyrics_worker: translation failed for {youtube_id}, persisting EN only: {e}"
                );
            }
        }

        // Step 4: Persist.
        let json_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let json_bytes = serde_json::to_vec(&track)?;
        tokio::fs::write(&json_path, &json_bytes).await?;
        mark_video_lyrics(&self.pool, video_id, true, Some(&final_source)).await?;

        tracing::info!("lyrics_worker: persisted lyrics for {youtube_id} (source={final_source})");
        Ok(())
    }

    /// Plan chunks → preprocess vocals → align → assemble. On any hard error,
    /// returns `Err((original_track, error))` so the caller can fall back to
    /// the line-level track without losing it.
    #[cfg_attr(test, mutants::skip)]
    async fn run_chunked_alignment(
        &self,
        python: &std::path::Path,
        audio: &std::path::Path,
        youtube_id: &str,
        track: LyricsTrack,
    ) -> std::result::Result<LyricsTrack, (LyricsTrack, anyhow::Error)> {
        let requests = chunking::plan_chunks(&track);
        if requests.is_empty() {
            return Ok(track);
        }

        let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
        if let Err(e) = aligner::preprocess_vocals(
            python,
            &self.script_path,
            &self.models_dir,
            audio,
            &wav_path,
        )
        .await
        {
            return Err((track, e));
        }

        let chunks_path = self.cache_dir.join(format!("{youtube_id}_chunks.json"));
        let out_path = self.cache_dir.join(format!("{youtube_id}_align_out.json"));
        let results = match aligner::align_chunks(
            python,
            &self.script_path,
            &wav_path,
            &requests,
            &chunks_path,
            &out_path,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => return Err((track, e)),
        };

        // Best-effort cleanup of the scratch WAV.
        let _ = tokio::fs::remove_file(&wav_path).await;

        let assembled = assembly::assemble(track, results);
        self.warn_on_degenerate_lines(&assembled, youtube_id);
        Ok(assembled)
    }

    /// Pure observability — emits a `warn!` per line whose alignment came
    /// back collapsed enough to suspect Mel-Roformer or the aligner failed.
    /// Skipped by mutation testing because the only behaviour is logging,
    /// which we don't unit-test against captured trace output.
    #[cfg_attr(test, mutants::skip)]
    fn warn_on_degenerate_lines(&self, track: &LyricsTrack, youtube_id: &str) {
        for (idx, line) in track.lines.iter().enumerate() {
            let pct = quality::duplicate_start_pct(line);
            if pct > DUPLICATE_START_WARN_PCT {
                warn!(
                    "lyrics_worker: degenerate alignment on {youtube_id} line {idx} ({pct:.1}% duplicate starts)"
                );
            }
        }
    }

    /// YT manual subs first, LRCLIB second, else bail.
    #[cfg_attr(test, mutants::skip)]
    async fn acquire_lyrics(
        &self,
        row: &crate::db::models::VideoLyricsRow,
    ) -> Result<(LyricsTrack, String)> {
        let youtube_id = &row.youtube_id;

        // 1. YouTube manual subs (skip on non-Windows / if ytdlp missing).
        let tmp = std::env::temp_dir().join("sp_yt_subs");
        let _ = tokio::fs::create_dir_all(&tmp).await;
        match youtube_subs::fetch_subtitles(&self.ytdlp_path, youtube_id, &tmp).await {
            Ok(Some(track)) => {
                info!("lyrics_worker: YT manual subs hit for {youtube_id}");
                return Ok((track, "yt_subs".to_string()));
            }
            Ok(None) => debug!("lyrics_worker: no YT manual subs for {youtube_id}"),
            Err(e) => warn!("lyrics_worker: YT sub fetch error for {youtube_id}: {e}"),
        }

        // 2. LRCLIB.
        if !row.song.is_empty() && !row.artist.is_empty() {
            let duration_s = row.duration_ms.map(|ms| (ms / 1000) as u32).unwrap_or(0);
            match lrclib::fetch_lyrics(&self.client, &row.artist, &row.song, duration_s).await {
                Ok(Some(track)) => {
                    info!("lyrics_worker: LRCLIB hit for {youtube_id}");
                    return Ok((track, "lrclib".to_string()));
                }
                Ok(None) => debug!("lyrics_worker: LRCLIB miss for {youtube_id}"),
                Err(e) => warn!("lyrics_worker: LRCLIB error for {youtube_id}: {e}"),
            }
        }

        anyhow::bail!("no lyrics source for {youtube_id}")
    }

    #[cfg_attr(test, mutants::skip)]
    async fn retry_missing_translations(&self) {
        if self.gemini_api_key.is_empty() {
            return;
        }
        {
            let backoff = self.retry_backoff.lock().await;
            if let Some(until) = backoff.silent_until
                && Instant::now() < until
            {
                return;
            }
        }
        let result = get_next_video_missing_translation(&self.pool, &self.cache_dir).await;
        let (_video_id, youtube_id) = match result {
            Ok(Some(pair)) => pair,
            _ => return,
        };
        let lyrics_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let content = match tokio::fs::read_to_string(&lyrics_path).await {
            Ok(c) => c,
            Err(e) => {
                debug!("lyrics retry: read failed for {youtube_id}: {e}");
                return;
            }
        };
        let mut track: LyricsTrack = match serde_json::from_str(&content) {
            Ok(t) => t,
            Err(e) => {
                debug!("lyrics retry: parse failed for {youtube_id}: {e}");
                return;
            }
        };
        info!("lyrics_worker: retrying translation for {youtube_id}");
        match translator::translate_lyrics(&self.gemini_api_key, &self.gemini_model, &mut track)
            .await
        {
            Ok(()) => {
                let json = serde_json::to_vec(&track).unwrap_or_default();
                let _ = tokio::fs::write(&lyrics_path, &json).await;
                info!("lyrics_worker: translation retry succeeded for {youtube_id}");
                let mut backoff = self.retry_backoff.lock().await;
                backoff.consecutive_failures = 0;
                backoff.silent_until = None;
            }
            Err(e) => {
                debug!("lyrics_worker: translation retry failed for {youtube_id}: {e}");
                let mut backoff = self.retry_backoff.lock().await;
                backoff.consecutive_failures = backoff.consecutive_failures.saturating_add(1);
                let attempt_index = backoff.consecutive_failures.saturating_sub(1).min(4);
                let secs = 60u64.saturating_mul(1u64 << attempt_index).min(600);
                backoff.silent_until = Some(Instant::now() + Duration::from_secs(secs));
                warn!(
                    "lyrics_worker: translation backoff for {secs}s after {} consecutive failures",
                    backoff.consecutive_failures
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Audit: retired symbols must not appear in this file.
    ///
    /// NOTE: banned symbol names are split across two string literals joined
    /// at runtime so this test file does not contain the verbatim string it is
    /// checking for (which would cause the test to always fail on itself).
    #[test]
    fn worker_has_no_retired_symbols() {
        let src = include_str!("worker.rs");
        let banned = [
            ["retry_missing", "_alignment"].concat(),
            ["count_duplicate", "_start_ms"].concat(),
            ["merge_word", "_timings"].concat(),
            ["ensure_progressive", "_words"].concat(),
            ["set_video", "_lyrics_source"].concat(),
            ["get_next_video_missing", "_alignment"].concat(),
        ];
        for sym in &banned {
            assert!(
                !src.contains(sym.as_str()),
                "worker.rs must not contain retired symbol `{sym}`"
            );
        }
        // The retired lyrics_source value must not appear as a literal.
        // Split to avoid self-match.
        let retired_source = ["\"lrclib", "+qwen3\""].concat();
        assert!(
            !src.contains(retired_source.as_str()),
            "worker.rs must not write the retired 'lrclib+qwen3' source literal"
        );
    }

    /// `acquire_lyrics` must call YouTube manual subs BEFORE LRCLIB. This
    /// is the single most important ordering decision in the pipeline —
    /// if LRCLIB wins for a song that has YT manual subs, the #148 E2E
    /// gate fails because `source == "lrclib"` instead of `yt_subs+qwen3`.
    #[test]
    fn acquire_lyrics_calls_youtube_subs_before_lrclib() {
        let src = include_str!("worker.rs");
        let body_start = src
            .find("async fn acquire_lyrics")
            .expect("acquire_lyrics must exist");
        let body = &src[body_start..];
        let yt_pos = body
            .find("youtube_subs::fetch_subtitles")
            .expect("acquire_lyrics must call youtube_subs::fetch_subtitles");
        let lrclib_pos = body
            .find("lrclib::fetch_lyrics")
            .expect("acquire_lyrics must call lrclib::fetch_lyrics");
        assert!(
            yt_pos < lrclib_pos,
            "YouTube subs fetch must happen before LRCLIB fetch in acquire_lyrics"
        );
    }
}
