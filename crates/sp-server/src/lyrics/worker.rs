//! Lyrics worker — tier-chain pipeline.
//!
//! Every song goes through:
//!   1. gather_sources: YT manual subs + LRCLIB + Genius + description in parallel.
//!   2. Vocal isolation (Mel-Roformer + anvuew; best-effort via `preprocess_vocals`).
//!   3. Build tier1 fetchers from candidate_texts → Orchestrator::process → AlignedTrack.
//!   4. Convert AlignedTrack → LyricsTrack via `align_track_to_lyrics_track`.
//!   5. SK translation — Claude (CLIProxyAPI) only per `feedback_claude_only_translation.md`.
//!   6. Persist JSON + DB row with pipeline_version.

use anyhow::Result;
use reqwest::Client;
use sp_core::lyrics::{LyricsLine, LyricsTrack, LyricsWord};
use sp_core::ws::{LyricsProcessingState, ServerMsg};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, error, info, warn};

use crate::{
    ai::client::AiClient,
    db::models::get_next_video_missing_translation,
    lyrics::{aligner, translator},
};

pub struct LyricsWorker {
    pool: SqlitePool,
    client: Client,
    cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    python_path: Option<PathBuf>,
    tools_dir: PathBuf,
    script_path: PathBuf,
    models_dir: PathBuf,
    /// Claude AI client for EN→SK translation (CLIProxyAPI).
    /// None if CLIProxyAPI is not configured.
    ai_client: Option<Arc<AiClient>>,
    venv_python: tokio::sync::RwLock<Option<PathBuf>>,
    retry_backoff: tokio::sync::Mutex<RetryBackoff>,
    /// Broadcast sender for lyrics-related WS events. Cloned from the app-wide
    /// event channel so messages reach all dashboard WS subscribers.
    events_tx: broadcast::Sender<ServerMsg>,
    /// Shared state read by `queue_update_loop` so the broadcast `processing`
    /// field reflects the current song being aligned.
    current_processing: Arc<RwLock<Option<LyricsProcessingState>>>,
}

#[derive(Default)]
struct RetryBackoff {
    silent_until: Option<Instant>,
    consecutive_failures: u32,
}

/// Re-export so `worker_tests` can keep importing from
/// `crate::lyrics::worker::gather_sources_impl`. The body lives in the
/// sibling `gather` module so `worker.rs` stays under the 1000-line cap.
pub(crate) use crate::lyrics::gather::gather_sources_impl;

impl LyricsWorker {
    pub fn new(
        pool: SqlitePool,
        cache_dir: PathBuf,
        ytdlp_path: PathBuf,
        python_path: Option<PathBuf>,
        tools_dir: PathBuf,
        ai_client: Option<Arc<AiClient>>,
        events_tx: broadcast::Sender<ServerMsg>,
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
            ai_client,
            venv_python: tokio::sync::RwLock::new(None),
            retry_backoff: tokio::sync::Mutex::new(RetryBackoff::default()),
            events_tx,
            current_processing: Arc::new(RwLock::new(None)),
        }
    }

    /// Snapshot the current processing state for use by queue_update_loop.
    // Arc clone; returning the shared handle has no behavior beyond reference-counting.
    #[cfg_attr(test, mutants::skip)]
    pub fn current_processing(&self) -> Arc<RwLock<Option<LyricsProcessingState>>> {
        self.current_processing.clone()
    }

    // I/O-only: updates shared RwLock + sends on broadcast channel. Fire-and-forget; no return value to assert.
    #[cfg_attr(test, mutants::skip)]
    #[allow(clippy::too_many_arguments)]
    async fn broadcast_stage(
        &self,
        video_id: i64,
        youtube_id: &str,
        song: &str,
        artist: &str,
        stage: &str,
        provider: Option<&str>,
        started_at_unix_ms: i64,
    ) {
        let state = LyricsProcessingState {
            video_id,
            youtube_id: youtube_id.into(),
            song: song.into(),
            artist: artist.into(),
            stage: stage.into(),
            provider: provider.map(|s| s.to_string()),
            started_at_unix_ms,
        };
        // Update shared state so the queue_update_loop's LyricsQueueUpdate carries it.
        *self.current_processing.write().await = Some(state.clone());
        // Fire the stage event for subscribers that want immediate transitions.
        let _ = self.events_tx.send(ServerMsg::LyricsProcessingStage {
            video_id,
            youtube_id: state.youtube_id,
            stage: state.stage,
            provider: state.provider,
        });
    }

    // Writes None to shared RwLock. Side effect verified via broadcast_stage/queue_update_loop integration.
    #[cfg_attr(test, mutants::skip)]
    async fn clear_processing(&self) {
        *self.current_processing.write().await = None;
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

        // Deploy the quality measurement script alongside the worker so CI
        // can run it on win-resolume to snapshot baseline vs post-deploy state.
        let measure_path = self.tools_dir.join("measure_lyrics_quality.py");
        tokio::fs::write(
            &measure_path,
            include_str!("../../../../scripts/measure_lyrics_quality.py"),
        )
        .await?;
        tracing::info!("lyrics_worker: wrote {}", measure_path.display());

        Ok(())
    }

    #[cfg_attr(test, mutants::skip)]
    pub async fn run(self, mut shutdown_rx: broadcast::Receiver<()>) {
        tracing::info!("lyrics_worker: started");

        if let Err(e) = self.ensure_script().await {
            error!("lyrics_worker: failed to write lyrics_worker.py: {e}");
        }

        // Start with a fresh yt-dlp scratch directory. Each song's
        // fetch_subtitles call writes a .json3 and then deletes it, but
        // a crash mid-fetch (or yt-dlp writing unexpected extra files
        // like .vtt fallbacks) can leak residue. Emptying the dir at
        // startup prevents unbounded growth across restarts.
        let yt_tmp = std::env::temp_dir().join("sp_yt_subs");
        let _ = tokio::fs::remove_dir_all(&yt_tmp).await;
        let _ = tokio::fs::create_dir_all(&yt_tmp).await;

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
        use crate::lyrics::{LYRICS_PIPELINE_VERSION, reprocess::get_next_video_for_lyrics};

        // Operational kill-switch. Read each tick so a live flip takes
        // effect within the 5 s worker poll window — essential during
        // events where Demucs/Gemini contention for CPU+GPU on the
        // shared win-resolume PC has caused reboots. Default "true" so
        // existing deploys and upgrades preserve current behavior.
        let enabled = crate::db::models::get_setting(&self.pool, "lyrics_worker_enabled")
            .await
            .ok()
            .flatten()
            .map(|v| v.trim().to_ascii_lowercase())
            .map(|v| !(v == "false" || v == "0" || v == "off" || v == "no"))
            .unwrap_or(true);
        if !enabled {
            debug!("worker: lyrics_worker_enabled=false, skipping this tick");
            return;
        }

        let row = match get_next_video_for_lyrics(&self.pool, LYRICS_PIPELINE_VERSION).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                self.retry_missing_translations().await;
                debug!("worker: nothing in priority queue");
                return;
            }
            Err(e) => {
                error!("worker: selector failed: {e}");
                return;
            }
        };
        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();
        tracing::info!(
            "worker: processing {} ({} - {})",
            youtube_id,
            row.artist,
            row.song
        );
        if let Err(e) = self.process_song(row).await {
            debug!("worker: processing failed for {youtube_id}: {e}");
            let _ = crate::db::models::mark_video_lyrics(
                &self.pool,
                video_id,
                false,
                Some("no_source"),
                crate::lyrics::LYRICS_PIPELINE_VERSION,
            )
            .await;
            self.clear_processing().await;
        }
    }

    /// Gather every available text + timing source for a song.
    /// Returns a `SongContext` ready for orchestrator. Never bails on a single
    /// source failure — collects what it can and returns; if zero text candidates
    /// were gathered, bails.
    #[cfg_attr(test, mutants::skip)] // orchestrates N I/O calls; covered by worker structural test `gather_sources_call_order_preserves_yt_subs_then_lrclib`
    async fn gather_sources(
        &self,
        row: &crate::db::models::VideoLyricsRow,
    ) -> Result<crate::lyrics::provider::SongContext> {
        // Read the Genius token fresh on every song so operators can add
        // the setting without restarting the server. Empty string disables
        // the Genius source entirely.
        let genius_token = crate::db::models::get_setting(&self.pool, "genius_access_token")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        gather_sources_impl(
            self.ai_client.as_deref(),
            &self.ytdlp_path,
            &self.cache_dir,
            &self.client,
            row,
            &genius_token,
        )
        .await
    }

    /// Apply per-line translations to `track`. Empty strings leave `sk = None`.
    #[cfg_attr(test, mutants::skip)]
    fn apply_translations(track: &mut LyricsTrack, translations: Vec<String>) {
        for (line, sk_text) in track.lines.iter_mut().zip(translations) {
            line.sk = if sk_text.is_empty() {
                None
            } else {
                Some(sk_text)
            };
        }
        track.language_translation = "sk".into();
    }

    /// EN→SK step of `process_song`. Silent on failure — UI degrades
    /// gracefully to English-only. Claude-only by design: the user pays a
    /// Max Plus subscription (unlimited at that tier) and Gemini quota is
    /// expensive + reserved for alignment. If Claude refuses with a policy
    /// response, the fix is to tune the prompt (see `translator::build_prompt`
    /// for the grandmother framing that defeats the copyright classifier),
    /// NOT to fall back to Gemini.
    #[cfg_attr(test, mutants::skip)]
    async fn translate_track(&self, track: &mut LyricsTrack, youtube_id: &str) {
        let Some(ai_client) = &self.ai_client else {
            return;
        };
        match translator::translate_via_claude(ai_client, track).await {
            Ok(translations) => Self::apply_translations(track, translations),
            Err(e) => warn!("worker: Claude translation failed for {youtube_id}: {e}"),
        }
    }

    #[cfg_attr(test, mutants::skip)]
    async fn process_song(&self, row: crate::db::models::VideoLyricsRow) -> Result<()> {
        use crate::lyrics::{
            LYRICS_PIPELINE_VERSION,
            orchestrator::{Orchestrator, OrchestratorInput},
            tier1::FetchFn,
            whisperx_replicate::WhisperXReplicateBackend,
        };

        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();
        let song = row.song.clone();
        let artist = row.artist.clone();

        let started_at_unix_ms = chrono::Utc::now().timestamp_millis();
        let start_instant = std::time::Instant::now();

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            "gathering",
            None,
            started_at_unix_ms,
        )
        .await;

        let ctx = match self.gather_sources(&row).await {
            Ok(c) => c,
            Err(e) => {
                self.clear_processing().await;
                return Err(e);
            }
        };

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            "preprocessing",
            None,
            started_at_unix_ms,
        )
        .await;

        // Preprocess vocals — UNCHANGED path (Mel-Roformer + anvuew dereverb).
        // Per feedback_winresolume_is_shared_event_machine.md this uses
        // BELOW_NORMAL priority subprocesses; not modified here.
        let venv_python = self.venv_python.read().await.clone();
        let clean_vocal: Option<PathBuf> = if let (Some(python), Some(audio_path)) = (
            venv_python.as_ref(),
            row.audio_file_path.as_ref().map(PathBuf::from),
        ) {
            if audio_path.exists() {
                let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
                match aligner::preprocess_vocals(
                    python,
                    &self.script_path,
                    &self.models_dir,
                    &audio_path,
                    &wav_path,
                )
                .await
                {
                    Ok(p) => Some(p),
                    Err(e) => {
                        warn!("worker: vocal isolation failed for {youtube_id}: {e}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            "aligning",
            None,
            started_at_unix_ms,
        )
        .await;

        // Build Tier-1 fetchers from candidate_texts gathered above.
        // Each provider::CandidateText becomes a FetchFn closure that immediately
        // returns the pre-fetched candidate without any additional I/O.
        let fetchers: Vec<FetchFn> = ctx
            .candidate_texts
            .into_iter()
            .map(|c| {
                let t1c = crate::lyrics::tier1::CandidateText::from(c);
                let boxed = Arc::new(t1c);
                let f: FetchFn = Arc::new(move || {
                    let candidate = (*boxed).clone();
                    Box::pin(async move { Some(candidate) })
                });
                f
            })
            .collect();

        // Build the WhisperX backend using the Replicate API token from settings.
        // Read per-song so operators can add the token without restarting.
        let replicate_token = crate::db::models::get_setting(&self.pool, "replicate_api_token")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        if replicate_token.trim().is_empty() {
            warn!(
                youtube_id = %youtube_id,
                "worker: replicate_api_token not set — skipping song; \
                 configure via PATCH /api/v1/settings"
            );
            self.clear_processing().await;
            return Err(anyhow::anyhow!("replicate_api_token not configured"));
        }
        let backend: Arc<dyn crate::lyrics::backend::AlignmentBackend> =
            Arc::new(WhisperXReplicateBackend::new(replicate_token));

        // Require an AI client for orchestrator construction — claude-merge needs it.
        // If None at runtime, log warning and bail processing for this song.
        let ai_client = match &self.ai_client {
            Some(c) => c.clone(),
            None => {
                warn!(
                    "worker: ai_client is None — CLIProxyAPI not configured; \
                     cannot run claude-merge for {youtube_id}"
                );
                self.clear_processing().await;
                return Err(anyhow::anyhow!(
                    "ai_client required for orchestrator (claude-merge) but is None"
                ));
            }
        };

        let orch = Orchestrator::new(
            backend,
            ai_client,
            crate::lyrics::line_splitter::SplitConfig::default(),
        );
        let aligned = match orch
            .process(OrchestratorInput {
                fetchers,
                language: "en",
                vocal_wav: clean_vocal.as_deref(),
            })
            .await
        {
            Ok(t) => t,
            Err(e) => {
                warn!("worker: orchestrator failed for {youtube_id}: {e}");
                // Vocals WAV intentionally preserved on disk — aligner's
                // cache-hit path (aligner.rs:87-96) reuses it on next run,
                // saving Demucs minutes per song. Self-heal removes orphans
                // when the parent video is removed (cache.rs).
                self.clear_processing().await;
                return Err(anyhow::anyhow!("orchestrator: {e}"));
            }
        };

        // Convert AlignedTrack → LyricsTrack at the worker boundary.
        let mut track = align_track_to_lyrics_track(aligned, LYRICS_PIPELINE_VERSION);

        // Vocals WAV intentionally preserved on disk — aligner's cache-hit
        // path (aligner.rs:87-96) reuses it on next run, saving Demucs
        // minutes per song. Self-heal removes orphans (cache.rs) when the
        // parent video is removed.

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            "translating",
            None,
            started_at_unix_ms,
        )
        .await;

        // EN→SK translation — Claude-only (per feedback_claude_only_translation.md).
        self.translate_track(&mut track, &youtube_id).await;

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            "persisting",
            None,
            started_at_unix_ms,
        )
        .await;

        // Persist JSON + DB row with pipeline_version.
        // quality_score is None (no audit log in the new pipeline).
        // Passing None writes SQL NULL — avoids poisoning ORDER BY
        // lyrics_quality_score ASC NULLS FIRST in the queue selector.
        let json_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let json_bytes = serde_json::to_vec(&track)?;
        tokio::fs::write(&json_path, &json_bytes).await?;

        crate::db::models::mark_video_lyrics_complete(
            &self.pool,
            video_id,
            &track.source,
            LYRICS_PIPELINE_VERSION,
            None,
        )
        .await?;

        tracing::info!(
            "worker: persisted {} (source={}, version={})",
            youtube_id,
            track.source,
            LYRICS_PIPELINE_VERSION
        );

        // Broadcast completion and clear processing state.
        let duration_ms = start_instant.elapsed().as_millis() as u64;
        let _ = self.events_tx.send(ServerMsg::LyricsCompleted {
            video_id,
            youtube_id: youtube_id.clone(),
            source: track.source.clone(),
            quality_score: 0.0,
            provider_count: 1,
            duration_ms,
        });
        self.clear_processing().await;

        Ok(())
    }

    #[cfg_attr(test, mutants::skip)]
    async fn retry_missing_translations(&self) {
        if self.ai_client.is_none() {
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

        let Some(ai_client) = &self.ai_client else {
            return;
        };
        // Claude-only by design (see `translate_track` doc comment).
        let result: Result<()> = match translator::translate_via_claude(ai_client, &track).await {
            Ok(t) => {
                Self::apply_translations(&mut track, t);
                Ok(())
            }
            Err(e) => Err(e),
        };

        match result {
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

/// Convert the orchestrator's internal `AlignedTrack` to the persisted
/// `sp_core::lyrics::LyricsTrack` shape at the worker boundary.
///
/// Mapping:
/// - `AlignedLine.text`     → `LyricsLine.en`
/// - `AlignedLine.start_ms` (u32) → `LyricsLine.start_ms` (u64) via widening
/// - `AlignedLine.end_ms`   (u32) → `LyricsLine.end_ms`   (u64) via widening
/// - `AlignedLine.words`    → `LyricsLine.words` — each `AlignedWord` maps to
///   `LyricsWord { text, start_ms: w.start_ms as u64, end_ms: w.end_ms as u64 }`.
///   Per `feedback_line_timing_only.md` the orchestrator ships `words: None` for
///   Tier-1 line-synced output; this function preserves whatever the orchestrator
///   produced (no word synthesis).
/// - `AlignedTrack.provenance` → `LyricsTrack.source`
/// - `version` is the caller-supplied LYRICS_PIPELINE_VERSION (not bumped here).
/// - `sk` and `language_translation` left empty — the worker fills them via
///   `translate_track` immediately after this call.
pub fn align_track_to_lyrics_track(
    aligned: crate::lyrics::backend::AlignedTrack,
    version: u32,
) -> LyricsTrack {
    let lines: Vec<LyricsLine> = aligned
        .lines
        .into_iter()
        .map(|l| LyricsLine {
            start_ms: l.start_ms as u64,
            end_ms: l.end_ms as u64,
            en: l.text,
            sk: None,
            words: l.words.map(|ws| {
                ws.into_iter()
                    .map(|w| LyricsWord {
                        text: w.text,
                        start_ms: w.start_ms as u64,
                        end_ms: w.end_ms as u64,
                    })
                    .collect()
            }),
        })
        .collect();
    LyricsTrack {
        version,
        source: aligned.provenance,
        language_source: "en".into(),
        language_translation: String::new(),
        lines,
    }
}

/// Broadcast lyrics queue counts on a 2-second interval until shutdown.
/// Consumed by the dashboard /lyrics page via WebSocket.
#[cfg_attr(test, mutants::skip)] // I/O-only; covered by end-to-end tests
pub async fn queue_update_loop(
    pool: sqlx::SqlitePool,
    events_tx: tokio::sync::broadcast::Sender<sp_core::ws::ServerMsg>,
    current_processing: Arc<RwLock<Option<LyricsProcessingState>>>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    use crate::lyrics::LYRICS_PIPELINE_VERSION;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => break,
            _ = interval.tick() => {
                if let Ok((b0, b1, b2)) =
                    crate::api::lyrics::fetch_queue_counts(&pool, LYRICS_PIPELINE_VERSION).await
                {
                    let processing = current_processing.read().await.clone();
                    let _ = events_tx.send(sp_core::ws::ServerMsg::LyricsQueueUpdate {
                        bucket0_count: b0,
                        bucket1_count: b1,
                        bucket2_count: b2,
                        pipeline_version: LYRICS_PIPELINE_VERSION,
                        processing,
                    });
                }
            }
        }
    }
}

#[path = "worker_tests.rs"]
#[cfg(test)]
mod tests;
