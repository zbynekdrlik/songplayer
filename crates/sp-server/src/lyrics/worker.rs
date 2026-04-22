//! Lyrics worker orchestrator — unified ensemble pipeline.
//!
//! Every song goes through the same path:
//!   1. gather_sources: YT manual subs + LRCLIB + autosub json3 in parallel.
//!   2. Vocal isolation (Qwen3 provider; best-effort).
//!   3. Orchestrator::process_song → LyricsTrack.
//!   4. SK translation (Claude only; Gemini quota reserved for alignment).
//!   5. Persist JSON + DB row with pipeline_version + quality_score.

use anyhow::Result;
use reqwest::Client;
use sp_core::lyrics::LyricsTrack;
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
    lyrics::{aligner, lrclib, translator, youtube_subs},
};

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
    /// Claude Opus AI client for translation + ensemble merge.
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

/// Free function containing the `gather_sources` logic so it can be tested
/// without constructing a full `LyricsWorker`.
///
/// mutants::skip: legacy LRCLIB guards (lines 99-100) + description match guard are
/// exercised end-to-end by `gather_sources_pushes_description_candidate_when_claude_returns_lyrics`
/// and `gather_sources_skips_description_when_claude_returns_empty_array` integration tests
/// (plus the structural call-order test further down); individual mutations in these
/// I/O-bound branches cannot be killed by unit tests without a full mock harness for
/// yt-dlp/LRCLIB/autosub, which is out of scope.
#[cfg_attr(test, mutants::skip)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn gather_sources_impl(
    ai_client: Option<&crate::ai::client::AiClient>,
    ytdlp_path: &std::path::Path,
    cache_dir: &std::path::Path,
    client: &reqwest::Client,
    row: &crate::db::models::VideoLyricsRow,
    autosub_tmp_dir: &std::path::Path,
) -> Result<crate::lyrics::provider::SongContext> {
    use crate::lyrics::autosub_provider::fetch_autosub;
    use crate::lyrics::provider::{CandidateText, SongContext};

    let youtube_id = row.youtube_id.clone();
    let audio_path = row
        .audio_file_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_default();

    // 1. Manual yt_subs
    let yt_tmp = std::env::temp_dir().join("sp_yt_subs");
    let _ = tokio::fs::create_dir_all(&yt_tmp).await;
    let yt_subs_track = match youtube_subs::fetch_subtitles(ytdlp_path, &youtube_id, &yt_tmp).await
    {
        Ok(Some(track)) => {
            info!("gather: YT manual subs hit for {youtube_id}");
            Some(track)
        }
        Ok(None) => {
            debug!("gather: no YT manual subs for {youtube_id}");
            None
        }
        Err(e) => {
            warn!("gather: YT sub fetch error for {youtube_id}: {e}");
            None
        }
    };

    // 2. LRCLIB (if song/artist known)
    let lrclib_track = if !row.song.is_empty() && !row.artist.is_empty() {
        let duration_s = row.duration_ms.map(|ms| (ms / 1000) as u32).unwrap_or(0);
        match lrclib::fetch_lyrics(client, &row.artist, &row.song, duration_s).await {
            Ok(Some(track)) => {
                info!("gather: LRCLIB hit for {youtube_id}");
                Some(track)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("gather: LRCLIB error for {youtube_id}: {e}");
                None
            }
        }
    } else {
        None
    };

    // 3. Auto-sub json3
    let autosub_json3 = match fetch_autosub(ytdlp_path, &youtube_id, autosub_tmp_dir).await {
        Ok(Some(p)) => Some(p),
        Ok(None) => None,
        Err(e) => {
            warn!("gather: autosub fetch error for {youtube_id}: {e}");
            None
        }
    };

    let mut candidate_texts: Vec<CandidateText> = Vec::new();
    if let Some(t) = &yt_subs_track {
        candidate_texts.push(CandidateText {
            source: "yt_subs".into(),
            lines: t.lines.iter().map(|l| l.en.clone()).collect(),
            has_timing: true,
            line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
        });
    }
    if let Some(t) = &lrclib_track {
        candidate_texts.push(CandidateText {
            source: "lrclib".into(),
            lines: t.lines.iter().map(|l| l.en.clone()).collect(),
            has_timing: true,
            line_timings: Some(t.lines.iter().map(|l| (l.start_ms, l.end_ms)).collect()),
        });
    }

    // 4. YouTube description lyrics (LLM-extracted). Best-effort.
    if let Some(ai) = ai_client {
        let description_lines = match crate::lyrics::description_provider::fetch_description_lyrics(
            ai,
            ytdlp_path,
            &youtube_id,
            cache_dir,
            &row.song,
            &row.artist,
        )
        .await
        {
            Ok(Some(lines)) if !lines.is_empty() => {
                info!(
                    youtube_id = %youtube_id,
                    line_count = lines.len(),
                    "gather: description lyrics hit"
                );
                Some(lines)
            }
            Ok(_) => {
                debug!("gather: no description lyrics for {youtube_id}");
                None
            }
            Err(e) => {
                warn!("gather: description fetch error for {youtube_id}: {e}");
                None
            }
        };
        if let Some(lines) = description_lines {
            candidate_texts.push(CandidateText {
                source: "description".into(),
                lines,
                has_timing: false,
                line_timings: None,
            });
        }
    }

    if candidate_texts.is_empty() {
        anyhow::bail!("no text sources available for {youtube_id}");
    }

    Ok(SongContext {
        video_id: youtube_id,
        audio_path,
        clean_vocal_path: None, // filled by process_song before orchestrator call
        candidate_texts,
        autosub_json3,
        duration_ms: row.duration_ms.unwrap_or(0) as u64,
    })
}

impl LyricsWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: SqlitePool,
        cache_dir: PathBuf,
        ytdlp_path: PathBuf,
        python_path: Option<PathBuf>,
        tools_dir: PathBuf,
        gemini_api_key: String,
        gemini_model: String,
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
            gemini_api_key,
            gemini_model,
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
    #[cfg_attr(test, mutants::skip)] // orchestrates N I/O calls; covered by worker structural test `gather_sources_call_order_preserves_yt_subs_then_lrclib_then_autosub`
    async fn gather_sources(
        &self,
        row: &crate::db::models::VideoLyricsRow,
        autosub_tmp_dir: &std::path::Path,
    ) -> Result<crate::lyrics::provider::SongContext> {
        gather_sources_impl(
            self.ai_client.as_deref(),
            &self.ytdlp_path,
            &self.cache_dir,
            &self.client,
            row,
            autosub_tmp_dir,
        )
        .await
    }

    /// EN→SK translation via Claude. No Gemini fallback — Gemini quota is
    /// reserved for alignment (which dominates call volume). If Claude fails,
    /// the track ships without translation; the UI degrades gracefully.
    #[cfg_attr(test, mutants::skip)]
    async fn translate_track(&self, track: &mut LyricsTrack, youtube_id: &str) {
        let Some(ai_client) = &self.ai_client else {
            return;
        };
        match translator::translate_via_claude(ai_client, track).await {
            Ok(translations) => {
                for (line, sk_text) in track.lines.iter_mut().zip(translations) {
                    line.sk = if sk_text.is_empty() {
                        None
                    } else {
                        Some(sk_text)
                    };
                }
                track.language_translation = "sk".into();
            }
            Err(e) => {
                warn!("worker: Claude translation failed for {youtube_id}: {e}");
            }
        }
    }

    /// Read quality score from the audit log written by the orchestrator.
    // Reads a JSON audit log file; graceful fallback to None on any error. Path/shape covered by orchestrator tests.
    #[cfg_attr(test, mutants::skip)]
    async fn read_quality_from_audit(&self, youtube_id: &str) -> Option<f32> {
        use crate::lyrics::reprocess::compute_quality_score;
        let audit_path = self
            .cache_dir
            .join(format!("{youtube_id}_alignment_audit.json"));
        let raw = tokio::fs::read_to_string(&audit_path).await.ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let qm = parsed.get("quality_metrics")?;
        let avg = qm.get("avg_confidence")?.as_f64()? as f32;
        let dup = qm.get("duplicate_start_pct")?.as_f64()? as f32;
        Some(compute_quality_score(avg, dup))
    }

    /// Read `reference_text_source` from the per-song alignment audit
    /// log. Surfaces the text-merge layer's decision (`description`,
    /// `lrclib`, `yt_subs`, `merged:<a>+<b>`) so operators can see it
    /// in the `worker: persisted` log line without having to open the
    /// audit JSON. Returns `None` when the audit log is missing or
    /// malformed.
    // Thin I/O wrapper around tokio::fs::read_to_string + serde_json;
    // mutation-testing can't meaningfully pin the error paths without a
    // real audit file fixture — behaviour covered by production logs.
    #[cfg_attr(test, mutants::skip)]
    async fn read_ref_source_from_audit(&self, youtube_id: &str) -> Option<String> {
        let audit_path = self
            .cache_dir
            .join(format!("{youtube_id}_alignment_audit.json"));
        let raw = tokio::fs::read_to_string(&audit_path).await.ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
        parsed
            .get("reference_text_source")?
            .as_str()
            .map(String::from)
    }

    #[cfg_attr(test, mutants::skip)]
    async fn process_song(&self, row: crate::db::models::VideoLyricsRow) -> Result<()> {
        use crate::lyrics::{
            LYRICS_PIPELINE_VERSION, orchestrator::Orchestrator, qwen3_provider::Qwen3Provider,
        };

        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();
        let song = row.song.clone();
        let artist = row.artist.clone();

        let started_at_unix_ms = chrono::Utc::now().timestamp_millis();
        let start_instant = std::time::Instant::now();

        let ai_client = self.ai_client.clone().ok_or_else(|| {
            anyhow::anyhow!("ai_client not configured; ensemble pipeline requires Claude")
        })?;

        // Dedicated per-song tmp dir for autosub json3 — cleaned on success.
        let autosub_tmp = std::env::temp_dir().join(format!("sp_autosub_{youtube_id}"));
        let _ = tokio::fs::create_dir_all(&autosub_tmp).await;

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

        let mut ctx = match self.gather_sources(&row, &autosub_tmp).await {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&autosub_tmp).await;
                self.clear_processing().await;
                return Err(e);
            }
        };

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            "text_merge",
            None,
            started_at_unix_ms,
        )
        .await;

        // Preprocess vocals for Qwen3 provider (best-effort; Qwen3 is skipped if this fails).
        let venv_python = self.venv_python.read().await.clone();
        let (python_for_qwen3, clean_vocal) = if let (Some(python), Some(audio_path)) = (
            venv_python.as_ref(),
            row.audio_file_path.as_ref().map(PathBuf::from),
        ) {
            if audio_path.exists() {
                let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
                let clean_vocal = match aligner::preprocess_vocals(
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
                };
                (Some(python.clone()), clean_vocal)
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        ctx.clean_vocal_path = clean_vocal;

        // Build provider list.
        // - GeminiProvider: registered when LYRICS_GEMINI_ENABLED AND at least one
        //   direct-API key is configured in `gemini_api_key` (comma-separated list).
        //   v14 transcribe_rotating cycles keys on HTTP 429.
        // - Qwen3Provider: registered only when LYRICS_QWEN3_ENABLED AND Python venv +
        //   clean vocal are available. Parked off; revived when word-level work resumes.
        // - AutoSubProvider: NOT registered as an alignment source (v16). Autosub's
        //   timing on sung music is unreliable and contaminated ensemble outputs
        //   in v11-v15; per user direction it is banned from alignment.
        use crate::lyrics::{
            LYRICS_GEMINI_ENABLED, LYRICS_QWEN3_ENABLED, gemini_client::GeminiClient,
            gemini_provider::GeminiProvider,
        };
        let mut providers: Vec<Box<dyn crate::lyrics::provider::AlignmentProvider>> = Vec::new();
        // v19: YtManualSubsProvider ships first. If the gather phase produced a
        // yt_subs candidate with line-level timing, alignment short-circuits
        // here — no Gemini API call, no ffmpeg chunking. The provider's
        // can_provide() returns false when no such candidate exists, so
        // normal Gemini-on-audio alignment runs for every other song.
        // Manual subs only per feedback_no_autosub.md; autosub never reaches
        // candidate_texts with has_timing=true in the current gather code.
        providers.push(Box::new(
            crate::lyrics::yt_manual_subs_provider::YtManualSubsProvider,
        ));
        // v16: AutoSubProvider is NOT registered as an alignment provider.
        // YouTube auto-captions have unreliable timing on sung music and
        // contaminate `ensemble:*` source tags. Autosub stays available for
        // text-candidate gathering in `gather_sources` (where it feeds the
        // description/text pool), but NEVER as an alignment source. See the
        // `feedback_no_autosub` memory for the user's explicit direction.
        if LYRICS_GEMINI_ENABLED {
            let ffmpeg_name = if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            };
            let ffmpeg_path = self.tools_dir.join(ffmpeg_name);
            let model = std::env::var("GEMINI_LYRICS_MODEL")
                .unwrap_or_else(|_| "gemini-3.1-pro-preview".to_string());
            // v14: direct API with one client per key. The `gemini_api_key`
            // DB setting is a comma-separated list of API keys; empty entries
            // and whitespace are stripped. `max_attempts=1` per client so a
            // 429 fails fast and the provider-level rotation in
            // `transcribe_rotating` advances to the next key immediately.
            let keys: Vec<String> = self
                .gemini_api_key
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let clients: Vec<GeminiClient> = keys
                .into_iter()
                .map(|k| {
                    let mut c = GeminiClient::direct(k, model.clone());
                    c.max_attempts = 1;
                    c
                })
                .collect();
            if !clients.is_empty() {
                tracing::info!(
                    "gemini: registered {} API key(s) for alignment rotation",
                    clients.len()
                );
                providers.push(Box::new(GeminiProvider {
                    clients,
                    current_key_idx: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    ffmpeg_path,
                    cache_dir: self.cache_dir.clone(),
                }));
            } else {
                tracing::warn!(
                    "gemini: LYRICS_GEMINI_ENABLED is true but gemini_api_key setting is empty \
                     — Gemini provider not registered"
                );
            }
        }
        if LYRICS_QWEN3_ENABLED {
            if let Some(python) = python_for_qwen3 {
                providers.push(Box::new(Qwen3Provider {
                    python_path: python,
                    script_path: self.script_path.clone(),
                    models_dir: self.models_dir.clone(),
                }));
            }
        }

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

        let orch = Orchestrator::new(providers, ai_client.clone(), self.cache_dir.clone());
        let mut track = match orch.process_song(&ctx).await {
            Ok(t) => t,
            Err(e) => {
                warn!("worker: ensemble failed for {youtube_id}: {e}");
                let _ = tokio::fs::remove_dir_all(&autosub_tmp).await;

                // Zero-provider fallback: emit a line-level track from the first
                // candidate with line timings. Preserves legacy LRCLIB-line-level behavior.
                let fallback = ctx
                    .candidate_texts
                    .iter()
                    .find(|c| c.has_timing && c.line_timings.is_some());
                let Some(c) = fallback else {
                    self.clear_processing().await;
                    return Err(e);
                };
                info!(
                    "worker: zero-provider fallback for {youtube_id} using source={}",
                    c.source
                );
                let timings = c.line_timings.as_ref().unwrap();
                let lines: Vec<sp_core::lyrics::LyricsLine> = c
                    .lines
                    .iter()
                    .zip(timings.iter())
                    .map(|(text, (start, end))| sp_core::lyrics::LyricsLine {
                        start_ms: *start,
                        end_ms: *end,
                        en: text.clone(),
                        sk: None,
                        words: None,
                    })
                    .collect();
                sp_core::lyrics::LyricsTrack {
                    version: 2,
                    source: c.source.clone(),
                    language_source: "en".into(),
                    language_translation: String::new(),
                    lines,
                }
            }
        };

        // Cleanup scratch files.
        let _ = tokio::fs::remove_dir_all(&autosub_tmp).await;
        let wav_path = self.cache_dir.join(format!("{youtube_id}_vocals16k.wav"));
        let _ = tokio::fs::remove_file(&wav_path).await;

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

        // SK translation (Claude → Gemini fallback) — unchanged logic, just extracted.
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

        // Persist JSON + DB row with pipeline_version + quality_score.
        let json_path = self.cache_dir.join(format!("{youtube_id}_lyrics.json"));
        let json_bytes = serde_json::to_vec(&track)?;
        tokio::fs::write(&json_path, &json_bytes).await?;

        // Recover quality from the audit log the orchestrator wrote.
        // None is returned when the audit log is absent (e.g. ensemble timeout fallback).
        // Passing None to mark_video_lyrics_complete writes SQL NULL instead of 0.0,
        // which avoids poisoning the ORDER BY lyrics_quality_score ASC NULLS FIRST selector.
        let quality_score: Option<f32> = self.read_quality_from_audit(&youtube_id).await;
        // Surface the reference-text source (description / lrclib / yt_subs /
        // merged:*) in the persist log so operators can see the text-merge
        // decision without opening the per-song audit JSON. `track.source`
        // only names the ALIGNMENT provider (qwen3 / ensemble:*), which hides
        // the text candidate that fed the aligner.
        let ref_source: String = self
            .read_ref_source_from_audit(&youtube_id)
            .await
            .unwrap_or_else(|| "unknown".into());

        crate::db::models::mark_video_lyrics_complete(
            &self.pool,
            video_id,
            &track.source,
            LYRICS_PIPELINE_VERSION,
            quality_score,
        )
        .await?;

        tracing::info!(
            "worker: persisted {} (source={}, ref_source={}, quality={}, version={})",
            youtube_id,
            track.source,
            ref_source,
            quality_score
                .map(|q| format!("{q:.2}"))
                .unwrap_or_else(|| "null".into()),
            LYRICS_PIPELINE_VERSION
        );

        // Broadcast completion and clear processing state.
        let provider_count = tokio::fs::read_to_string(
            &self
                .cache_dir
                .join(format!("{youtube_id}_alignment_audit.json")),
        )
        .await
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("providers_run")
                .and_then(|p| p.as_array())
                .map(|a| a.len())
        })
        .unwrap_or(0) as u8;
        let duration_ms = start_instant.elapsed().as_millis() as u64;
        let _ = self.events_tx.send(ServerMsg::LyricsCompleted {
            video_id,
            youtube_id: youtube_id.clone(),
            source: track.source.clone(),
            quality_score: quality_score.unwrap_or(0.0),
            provider_count,
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

        // Claude-only. Gemini quota is reserved for alignment.
        let Some(ai_client) = &self.ai_client else {
            return;
        };
        let result = match translator::translate_via_claude(ai_client, &track).await {
            Ok(translations) => {
                for (line, sk_text) in track.lines.iter_mut().zip(translations) {
                    line.sk = if sk_text.is_empty() {
                        None
                    } else {
                        Some(sk_text)
                    };
                }
                track.language_translation = "sk".to_string();
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
