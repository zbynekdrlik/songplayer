//! Lyrics worker — two-tier pipeline (v22, #159).
//!
//! Every song goes through:
//!   1. gather_sources: YT manual subs + LRCLIB + Genius + description in parallel.
//!   2. Vocal isolation (Mel-Roformer + anvuew; best-effort via `preprocess_vocals`).
//!   3. Tier 1 (★): v21 mtl forced-alignment reference stage
//!      (`run_mtl_reference_stage`) — gate PASS ships mtl line timings.
//!   4. Tier 2 (base): g35t transcript (`run_g35t_transcript_branch`) for every
//!      song the reference stage did not ship.
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
    lyrics::{translator, worker_outcome::SongOutcome},
};

pub struct LyricsWorker {
    pub(crate) pool: SqlitePool,
    pub(crate) client: Client,
    pub(crate) cache_dir: PathBuf,
    ytdlp_path: PathBuf,
    python_path: Option<PathBuf>,
    pub(crate) tools_dir: PathBuf,
    pub(crate) script_path: PathBuf,
    pub(crate) models_dir: PathBuf,
    /// Claude AI client for EN→SK translation (CLIProxyAPI).
    /// None if CLIProxyAPI is not configured.
    pub(crate) ai_client: Option<Arc<AiClient>>,
    pub(crate) venv_python: tokio::sync::RwLock<Option<PathBuf>>,
    // pub(crate) so the sibling `worker_translation` module (#152) shares the
    // same Claude-translation backoff gate as `retry_missing_translations`.
    pub(crate) retry_backoff: tokio::sync::Mutex<RetryBackoff>,
    /// Broadcast sender for lyrics-related WS events. Cloned from the app-wide
    /// event channel so messages reach all dashboard WS subscribers.
    pub(crate) events_tx: broadcast::Sender<ServerMsg>,
    /// Spotify track ID auto-resolver. Constructed once at worker startup.
    /// Per-song, the worker checks the gate (spotify_track_id IS NULL AND
    /// spotify_resolved_at IS NULL) before invoking it.
    spotify_resolver: crate::lyrics::spotify_resolver::SpotifyResolver,
    /// Shared state read by `queue_update_loop` so the broadcast `processing`
    /// field reflects the current song being aligned. pub(crate) so the #154
    /// idle-gate seam (`idle_gate.rs`) can surface the "waiting — wall in use"
    /// state through the same field.
    pub(crate) current_processing: Arc<RwLock<Option<LyricsProcessingState>>>,
    /// #154 idle gate: engine health registry, read for the per-pipeline
    /// `Playing` state (the same snapshots `/api/v1/ndi/health` serves) so heavy
    /// GPU/CPU work is deferred while an output is on the wall. `None` in unit
    /// tests that don't exercise the gate.
    pub(crate) ndi_health_registry: Option<Arc<crate::playback::ndi_health::NdiHealthRegistry>>,
    /// #154 idle gate: shared OBS state, read for `streaming`/`recording` so
    /// heavy work is also deferred while OBS is live. `None` in unit tests.
    pub(crate) obs_state: Option<Arc<RwLock<crate::obs::ObsState>>>,
    /// #154 idle gate: once-per-transition log tracker so the "waiting — wall
    /// in use" INFO logs on each state change, not every 5-s tick.
    pub(crate) wall_gate_log: std::sync::Mutex<crate::lyrics::idle_gate::GateLog>,
}

#[derive(Default)]
pub(crate) struct RetryBackoff {
    pub(crate) silent_until: Option<Instant>,
    pub(crate) consecutive_failures: u32,
}

/// Re-export so `worker_tests` can keep importing from
/// `crate::lyrics::worker::gather_sources_impl`. The body lives in the
/// sibling `gather` module so `worker.rs` stays under the 1000-line cap.
pub(crate) use crate::lyrics::gather::gather_sources_impl;

/// Pure decision for the Spotify pre-gather hook (#73). Returns `true`
/// only when ALL four guards are satisfied:
/// - `spotify_track_id` is NULL (no resolved id yet)
/// - `spotify_resolved_at` is NULL (no prior attempt recorded)
/// - an AI client is configured (CLIProxyAPI present)
/// - both `song` and `artist` are non-empty
///
/// Extracted from `process_song` so the guards are pinned by unit tests
/// (#76) instead of relying on a regression to land before being noticed.
pub(crate) fn should_resolve_spotify(
    spotify_track_id: Option<&str>,
    spotify_resolved_at: Option<&str>,
    ai_client_present: bool,
    song: &str,
    artist: &str,
) -> bool {
    spotify_track_id.is_none()
        && spotify_resolved_at.is_none()
        && ai_client_present
        && !song.is_empty()
        && !artist.is_empty()
}

impl LyricsWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool: SqlitePool,
        cache_dir: PathBuf,
        ytdlp_path: PathBuf,
        python_path: Option<PathBuf>,
        tools_dir: PathBuf,
        ai_client: Option<Arc<AiClient>>,
        events_tx: broadcast::Sender<ServerMsg>,
        ndi_health_registry: Arc<crate::playback::ndi_health::NdiHealthRegistry>,
        obs_state: Arc<RwLock<crate::obs::ObsState>>,
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
            spotify_resolver: crate::lyrics::spotify_resolver::SpotifyResolver::new(),
            current_processing: Arc::new(RwLock::new(None)),
            ndi_health_registry: Some(ndi_health_registry),
            obs_state: Some(obs_state),
            wall_gate_log: std::sync::Mutex::new(crate::lyrics::idle_gate::GateLog::default()),
        }
    }

    /// Build a minimal LyricsWorker for unit tests. Fields not used by the
    /// asr_path branch get placeholder values; tests must not exercise
    /// downloader / tools / orchestrator paths against this instance.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        pool: SqlitePool,
        cache_dir: std::path::PathBuf,
        events_tx: broadcast::Sender<ServerMsg>,
    ) -> Self {
        use std::path::PathBuf;
        Self {
            pool,
            client: Client::new(),
            cache_dir: cache_dir.clone(),
            ytdlp_path: PathBuf::from("yt-dlp"),
            python_path: None,
            tools_dir: PathBuf::from("/tmp/tools"),
            script_path: PathBuf::from("/tmp/script"),
            models_dir: PathBuf::from("/tmp/models"),
            ai_client: None,
            venv_python: tokio::sync::RwLock::new(None),
            retry_backoff: tokio::sync::Mutex::new(RetryBackoff::default()),
            events_tx,
            spotify_resolver: crate::lyrics::spotify_resolver::SpotifyResolver::new(),
            current_processing: Arc::new(RwLock::new(None)),
            ndi_health_registry: None,
            obs_state: None,
            wall_gate_log: std::sync::Mutex::new(crate::lyrics::idle_gate::GateLog::default()),
        }
    }

    /// Snapshot the current processing state for use by queue_update_loop.
    // Arc clone; returning the shared handle has no behavior beyond reference-counting.
    #[cfg_attr(test, mutants::skip)]
    pub fn current_processing(&self) -> Arc<RwLock<Option<LyricsProcessingState>>> {
        self.current_processing.clone()
    }

    /// Attach the #154 idle-gate handles to a test worker so the gate can be
    /// exercised at the loop level with an injected `Playing` snapshot.
    #[cfg(test)]
    pub(crate) fn with_wall_handles(
        mut self,
        ndi_health_registry: Arc<crate::playback::ndi_health::NdiHealthRegistry>,
        obs_state: Arc<RwLock<crate::obs::ObsState>>,
    ) -> Self {
        self.ndi_health_registry = Some(ndi_health_registry);
        self.obs_state = Some(obs_state);
        self
    }

    // I/O-only: updates shared RwLock + sends on broadcast channel. Fire-and-forget; no return value to assert.
    #[cfg_attr(test, mutants::skip)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn broadcast_stage(
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
    pub(crate) async fn clear_processing(&self) {
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

        // Lever 2 (#143): deploy the mtl-alignment eval wrapper into the
        // tools dir the same way — embedded via `include_str!` at compile
        // time, written out on every worker startup so a version bump always
        // ships the latest wrapper. `MtlConfig::from_tools_dir` reads it
        // back from here; the venv (`mtl_aligner_venv`) and the vendored
        // `LyricsAlignment-MTL` repo are installed separately (win-resolume
        // ops, not this deploy path — see the `lyrics-pipeline` skill).
        let mtl_run_py_path = self.tools_dir.join("lyrics_alignment_mtl_run.py");
        tokio::fs::write(
            &mtl_run_py_path,
            include_str!("../../../../eval/lyrics/aligners/lyrics_alignment_mtl/run.py"),
        )
        .await?;
        tracing::info!("lyrics_worker: wrote {}", mtl_run_py_path.display());

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

        // Lever 2 (#143): one-time startup check, not per-song — a per-song
        // WARN would spam the log every 5s poll while tooling is absent.
        // `run_mtl_reference_stage` re-checks (cheap file-exists calls) and
        // logs at INFO per song when still unavailable.
        let mtl_cfg = crate::lyrics::mtl_aligner::MtlConfig::from_tools_dir(&self.tools_dir);
        if !mtl_cfg.is_available() {
            warn!(
                "lyrics_worker: reference stage (#143) disabled — mtl tooling not found under {} \
                 (expected mtl_aligner_venv/Scripts/python.exe, lyrics_alignment_mtl_run.py, \
                 LyricsAlignment-MTL)",
                self.tools_dir.display()
            );
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

        // #162 loop-level gate: only `idle-only` defers here (pre-#162 gate +
        // idle-settle); `low-priority` (default) never defers — heavy steps run
        // at reduced priority instead, so the queue drains continuously. On an
        // idle-only defer, surface "waiting — wall in use" and keep the cheap
        // non-gated translation moving; no DB deferral (row stays at the head).
        let mode = self.processing_mode().await;
        let (defer, activity) = self.loop_should_defer(mode).await;
        if defer {
            let detail = self.wall_busy_detail(activity).await;
            self.note_wall_gate(true, &detail);
            self.enter_wall_wait(&detail).await;
            // Translation is a cheap Claude HTTP call, explicitly NOT gated —
            // it keeps the SK backfill moving while the wall is in use.
            self.retry_missing_translations().await;
            self.retranslate_next_stale().await;
            return;
        }
        // Not deferring (low-priority always; idle-only when the wall is idle) —
        // log the resume transition (once) before picking work.
        self.note_wall_gate(false, "");

        let row = match get_next_video_for_lyrics(&self.pool, LYRICS_PIPELINE_VERSION).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                self.retry_missing_translations().await;
                // #152: queue empty → advance one stale-translation row (no alignment).
                self.retranslate_next_stale().await;
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
        match self.process_song(row).await {
            Ok(SongOutcome::Done) => {}
            // #144: durable retry backoff (mirrors downloader #140) so the
            // selector skips this unprocessable row until due instead of
            // re-picking it every 5 s tick (37-min hot-loop on 3_ccqgwVZYM).
            Ok(SongOutcome::Deferred(reason)) => {
                self.defer_song(video_id, &youtube_id, reason).await
            }
            // #154: the wall went busy mid-song (after isolation, before mtl).
            // No backoff penalty — the isolated vocal WAV is preserved on disk,
            // so the next idle pick is a cache-hit isolation + mtl with
            // identical output. The waiting stage is already broadcast; leave
            // current_processing as-is so the dashboard keeps showing it until
            // the next tick re-evaluates.
            Ok(SongOutcome::WaitingForWall) => {
                debug!("worker: {youtube_id} deferred — wall in use (no backoff)");
            }
            // #162: memory headroom fell below the floor before a heavy step —
            // deferred with NO backoff (the WARN with the numbers already fired
            // in `heavy_step_memory_ok`); the row re-runs the moment memory frees.
            Ok(SongOutcome::WaitingForMemory) => {
                debug!("worker: {youtube_id} deferred — memory headroom low (no backoff)");
            }
            Err(e) => {
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
        // the Genius fallback. Genius is only consulted when lyrics.ovh
        // returns no match (see gather.rs).
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

    // `apply_translations` + `translate_track` live in `worker_translation.rs` (#152, cap).

    #[cfg_attr(test, mutants::skip)]
    async fn process_song(
        &self,
        mut row: crate::db::models::VideoLyricsRow,
    ) -> Result<SongOutcome> {
        use crate::lyrics::LYRICS_PIPELINE_VERSION;

        let video_id = row.id;
        let youtube_id = row.youtube_id.clone();
        let song = row.song.clone();
        let artist = row.artist.clone();
        // Duration cap (#144): reject > 30-min live sets before any GPU work.
        if crate::lyrics::worker_outcome::exceeds_duration_cap(row.duration_ms) {
            return self.mark_over_cap(&row).await;
        }

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

        // Spotify auto-resolution gate (#73). Run Claude once per song lifetime
        // when `spotify_track_id` is NULL and we've never recorded an attempt.
        // The result (success OR no-match) is persisted with a timestamp so the
        // gate short-circuits on subsequent reprocesses. The four guards are
        // centralized in `should_resolve_spotify` so #76 unit tests can pin them.
        if should_resolve_spotify(
            row.spotify_track_id.as_deref(),
            row.spotify_resolved_at.as_deref(),
            self.ai_client.is_some(),
            &row.song,
            &row.artist,
        ) && !self.spotify_resolver.in_backoff()
        {
            // `ai_client` presence was verified above; this `if let` is
            // structurally infallible but keeps the borrow explicit.
            if let Some(ai_client) = self.ai_client.as_deref() {
                use crate::lyrics::spotify_resolver::ResolveOutcome;
                let outcome = self
                    .spotify_resolver
                    .resolve(ai_client, &row.song, &row.artist, &row.youtube_id)
                    .await;
                match outcome {
                    ResolveOutcome::Resolved(id) => {
                        if let Err(e) = crate::db::models::set_video_spotify_resolution(
                            &self.pool,
                            row.id,
                            Some(&id),
                        )
                        .await
                        {
                            warn!(
                                "worker: failed to persist resolved spotify_track_id for {}: {e}",
                                row.youtube_id
                            );
                        } else {
                            info!(
                                youtube_id = %row.youtube_id,
                                track_id = %id,
                                "spotify_resolver: resolved + verified"
                            );
                            row.spotify_track_id = Some(id);
                        }
                    }
                    ResolveOutcome::NoMatch => {
                        if let Err(e) = crate::db::models::set_video_spotify_resolution(
                            &self.pool, row.id, None,
                        )
                        .await
                        {
                            warn!(
                                "worker: failed to persist no-match for {}: {e}",
                                row.youtube_id
                            );
                        } else {
                            debug!(
                                youtube_id = %row.youtube_id,
                                "spotify_resolver: no canonical match"
                            );
                        }
                    }
                    ResolveOutcome::Error(e) => {
                        warn!(
                            "worker: spotify resolution transport error for {}: {e}",
                            row.youtube_id
                        );
                        // Intentionally do NOT persist resolved_at — leaves
                        // the row eligible for retry on the next worker pass.
                    }
                }
            }
        }

        let ctx = match self.gather_sources(&row).await {
            Ok(c) => c,
            Err(e) => {
                self.clear_processing().await;
                return Err(e);
            }
        };

        // v22 (#159): one regime. Every song with vocals + a text candidate
        // (≥4 lines) tries the v21 mtl reference stage below; songs it does
        // not ship take the g35t base tier.
        //
        // #162: read the processing mode ONCE per song. `low-priority` (default)
        // runs every heavy step at reduced priority (CPU-idle while the wall is
        // in use, GPU when idle); `idle-only` keeps the pre-#162 defer/settle/
        // abort gate. The badge suffix ` (cpu, wall in use)` surfaces the CPU
        // regime; the idle-only "waiting" badge is separate.
        let gpu_mem = self.gpu_mem_setting().await;
        let mode = self.processing_mode().await;
        let regime_activity = self.wall_activity().await;
        let stage_suffix = Self::stage_regime_suffix(mode, regime_activity);

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            &format!("preprocessing{stage_suffix}"),
            None,
            started_at_unix_ms,
        )
        .await;

        // #162: vocal isolation under the priority regime (details in
        // `isolate_with_regime`). `Err(HeavyDefer)` → the song defers with NO
        // backoff: idle-only wall-abort (WaitingForWall) or low memory
        // (WaitingForMemory); low-priority otherwise never Errs (it re-runs on
        // CPU internally). `defer_heavy` maps it to the right no-penalty outcome.
        let clean_vocal: Option<PathBuf> = match self
            .isolate_with_regime(&row, gpu_mem.as_deref(), mode)
            .await
        {
            Ok(v) => v,
            Err(d) => return Ok(self.defer_heavy(d).await),
        };

        self.broadcast_stage(
            video_id,
            &youtube_id,
            &song,
            &artist,
            &format!("aligning{stage_suffix}"),
            None,
            started_at_unix_ms,
        )
        .await;

        // Convert provider::CandidateText → tier1::CandidateText so the v21
        // reference stage can pick the best candidate. All I/O already happened
        // in `gather_sources`.
        let candidates: Vec<crate::lyrics::tier1::CandidateText> = ctx
            .candidate_texts
            .into_iter()
            .map(crate::lyrics::tier1::CandidateText::from)
            .collect();

        // Parse the Gemini key list once — used by BOTH the v21 reference gate
        // and the v22 g35t base tier.
        let gemini_csv = crate::db::models::get_setting(&self.pool, "gemini_api_key")
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let gemini_keys = crate::lyrics::g35t_client::gemini_keys_from_setting(&gemini_csv);

        // Tier 1 — v21 (#143) forced-alignment reference stage (★). Aligns the
        // best text candidate via mtl and verifies it against a g35t word
        // transcript. On gate PASS this ships the mtl line timings directly;
        // otherwise (skip / gate fail / mtl error) it returns None and the song
        // takes the g35t base tier below. UNCHANGED byte-for-byte from v21.
        let best_candidate =
            crate::lyrics::claude_merge::best_authoritative_candidate(&candidates).cloned();

        // #154 gate #2 (idle-only mode only, #162). Isolation above may have
        // started while the wall was idle and finished after it went busy — a
        // running subprocess is never killed (that would waste ~4 min of GPU
        // work), so the check goes here, BEFORE the next heavy spawn (mtl). If
        // the wall is busy now, defer the WHOLE song (WaitingForWall): the
        // isolated vocal WAV is preserved on disk, so the next idle pick is a
        // cache-hit isolation + mtl with byte-identical output. We do NOT fall
        // through to the g35t base tier — that would degrade the ★ mtl tier
        // (owner's quality-first rule). In LOW-PRIORITY mode there is no gate #2:
        // mtl runs at reduced priority instead (the backend picks the plan and
        // re-runs on CPU if a GPU job is aborted).
        if mode == crate::lyrics::heavy_plan::ProcessingMode::IdleOnly
            && best_candidate.is_some()
            && clean_vocal.is_some()
            && self.defer_before_mtl().await
        {
            return Ok(SongOutcome::WaitingForWall);
        }

        let reference_backend = crate::lyrics::orchestrator::RealReferenceStageBackend {
            mtl_cfg: crate::lyrics::mtl_aligner::MtlConfig::from_tools_dir(&self.tools_dir),
            work_dir: self.cache_dir.clone(),
            http_client: self.client.clone(),
            gemini_keys: gemini_keys.clone(),
            gpu_mem_setting: gpu_mem.clone(),
            // #161/#162: the backend wraps ONLY the mtl subprocess (never the
            // g35t HTTP verification), using these handles + the once-read
            // processing mode (which selects the mtl plan + abort arming).
            ndi_health_registry: self.ndi_health_registry.clone(),
            obs_state: self.obs_state.clone(),
            mode,
        };
        let mtl_track = match self
            .run_mtl_reference_stage(
                video_id,
                &youtube_id,
                best_candidate.as_ref(),
                clean_vocal.as_deref(),
                &reference_backend,
            )
            .await
        {
            Ok(t) => t,
            // #161 wall-abort / #162 low memory during/before mtl → defer the
            // whole song with NO penalty (defer_heavy). Never fall through to the
            // g35t base tier (that would degrade the ★ mtl tier, owner's
            // quality-first rule); the next pick re-runs mtl to byte-identical ★.
            Err(d) => return Ok(self.defer_heavy(d).await),
        };

        // Tier 2 — v22 (#159) g35t base tier. The single fallback for every
        // song the reference stage did not ship: a Gemini 3.5 Transcribe
        // transcript grouped into lines. Replaces the deleted WhisperX + asr_path
        // routes. Reuses the already-isolated `clean_vocal` (no second Demucs).
        let mut track = if let Some(t) = mtl_track {
            t
        } else {
            match self
                .run_g35t_transcript_branch(
                    clean_vocal.as_deref(),
                    // #171: mix FLAC — base-tier last resort when isolation never yields a vocal.
                    row.audio_file_path.as_deref().map(std::path::Path::new),
                    &gemini_keys,
                    video_id,
                    &youtube_id,
                    &song,
                    &artist,
                    started_at_unix_ms,
                )
                .await?
            {
                crate::lyrics::worker_g35t::G35tOutcome::Track(t) => t,
                crate::lyrics::worker_g35t::G35tOutcome::Deferred(reason) => {
                    // Vocals WAV intentionally preserved on disk — aligner's
                    // cache-hit path reuses it on the next run.
                    self.clear_processing().await;
                    return Ok(SongOutcome::Deferred(reason));
                }
                crate::lyrics::worker_g35t::G35tOutcome::Quarantined => {
                    self.clear_processing().await;
                    return Ok(SongOutcome::Done);
                }
            }
        };

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
        // #152: gender picks masculine (default) / feminine Slovak first-person forms.
        let gender = self.resolve_gender(video_id).await;
        self.translate_track(&mut track, &youtube_id, gender).await;

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

        // Persist JSON sidecar + DB row with pipeline_version, through the shared
        // writer (#182) the dub subtitle store also uses. quality_score is None
        // (no audit log in the new pipeline) — writes SQL NULL, avoiding poisoning
        // ORDER BY lyrics_quality_score ASC NULLS FIRST in the queue selector.
        crate::lyrics::track_store::persist_lyrics_track(
            &self.pool,
            &self.cache_dir,
            &youtube_id,
            video_id,
            &track,
            LYRICS_PIPELINE_VERSION,
        )
        .await?;

        // #152: mark this song translated under the current translation version
        // so the stale-translation retranslate pass skips it (independent of
        // lyrics_pipeline_version — no re-alignment).
        let _ = crate::db::models::stamp_translation_version(
            &self.pool,
            video_id,
            crate::lyrics::LYRICS_TRANSLATION_VERSION,
        )
        .await;

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

        Ok(SongOutcome::Done)
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
        let (video_id, youtube_id) = match result {
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
        // Claude-only by design (see `translate_track` doc comment). #152: the
        // song's gender picks masculine (default) / feminine Slovak forms.
        let gender = self.resolve_gender(video_id).await;
        let result: Result<()> =
            match translator::translate_via_claude(ai_client, &track, gender).await {
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
                // #152: stamp the translation version so the stale-translation
                // pass does not re-pick a song we just filled in.
                let _ = crate::db::models::stamp_translation_version(
                    &self.pool,
                    video_id,
                    crate::lyrics::LYRICS_TRANSLATION_VERSION,
                )
                .await;
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

#[path = "worker_tests_reference.rs"]
#[cfg(test)]
mod tests_reference;

#[path = "worker_tests_idle_gate.rs"]
#[cfg(test)]
mod tests_idle_gate;
