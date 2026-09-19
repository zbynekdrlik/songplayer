//! #132: runtime playlist-pipeline lifecycle for `PlaybackEngine`.
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap.
//!
//! At startup the engine pre-creates a pipeline for every active playlist
//! (`lib.rs::start`). These two methods extend the SAME registration to
//! playlists created / activated / deleted / deactivated at RUNTIME via the
//! API: `EngineCommand::EnsurePipeline` → [`ensure_pipeline_for_playlist`],
//! `EngineCommand::RemovePipeline` → [`remove_pipeline`]. Without them a
//! runtime-created playlist has a scene-map entry but no pipeline, so scene
//! detection logs `no pipeline for playlist` until a process restart.
//!
//! [`ensure_pipeline_for_playlist`]: PlaybackEngine::ensure_pipeline_for_playlist
//! [`remove_pipeline`]: PlaybackEngine::remove_pipeline

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tracing::{debug, info, warn};

use super::{
    PREVIOUS_HISTORY_CAPACITY, PlayState, PlaybackEngine, PlaybackMode, PlaybackPipeline,
    PlaylistPipeline,
};

impl PlaybackEngine {
    /// #132: Ensure a pipeline exists for a playlist created or activated at
    /// runtime (via the API), so scene detection can start playback without a
    /// process restart. Reconciles from the DB — a pipeline is (idempotently)
    /// created only when the playlist is active and has a non-empty NDI output
    /// name, mirroring the startup pre-create loop in `lib.rs::start`. Delegates
    /// to the idempotent [`ensure_pipeline`], so a runtime pipeline picks up the
    /// same engine-level `genlock_pacing` / `clock_health` / burn-registry
    /// configuration as a boot pipeline. No-op for a missing row, an inactive
    /// playlist, or an empty NDI name.
    ///
    /// [`ensure_pipeline`]: PlaybackEngine::ensure_pipeline
    pub async fn ensure_pipeline_for_playlist(&mut self, playlist_id: i64) {
        use sqlx::Row;
        let row = match sqlx::query("SELECT ndi_output_name, is_active FROM playlists WHERE id = ?")
            .bind(playlist_id)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => {
                debug!(
                    playlist_id,
                    "ensure_pipeline_for_playlist: no playlist row, ignoring"
                );
                return;
            }
            Err(e) => {
                warn!(playlist_id, %e, "ensure_pipeline_for_playlist: DB lookup failed");
                return;
            }
        };

        let ndi_name: String = row.get("ndi_output_name");
        let is_active = row.get::<i64, _>("is_active") != 0;

        if !is_active {
            debug!(
                playlist_id,
                "ensure_pipeline_for_playlist: playlist inactive, not creating pipeline"
            );
            return;
        }
        if ndi_name.is_empty() {
            debug!(
                playlist_id,
                "ensure_pipeline_for_playlist: empty NDI output name, not creating pipeline"
            );
            return;
        }

        info!(
            playlist_id,
            ndi_name, "ensuring pipeline for runtime-created/activated playlist"
        );
        // #196: record the sender's advertised URL for `/api/v1/ndi/health`
        // (idempotent; a no-op if the pipeline already exists).
        self.create_and_record_sender(playlist_id, &ndi_name).await;
    }

    /// #132: Tear down a playlist's pipeline after a runtime delete/deactivate.
    /// Removing it from the map drops the `PlaybackPipeline`, whose `Drop`
    /// sends `Shutdown` to its thread (destroying the NDI sender) — the same
    /// contract `run()`'s `pipelines.clear()` relies on. Also drops the
    /// playlist's genlock lock-state window. No-op if no pipeline exists.
    pub fn remove_pipeline(&mut self, playlist_id: i64) {
        match self.pipelines.remove(&playlist_id) {
            Some(_pp) => {
                self.lock_windows.remove(&playlist_id);
                info!(
                    playlist_id,
                    "removed playback pipeline (runtime delete/deactivate)"
                );
                // `_pp` (and its `PlaybackPipeline`) drops here → `Shutdown`
                // is sent to the pipeline thread.
            }
            None => {
                debug!(
                    playlist_id,
                    "remove_pipeline: no pipeline for playlist, ignoring"
                );
            }
        }
    }

    /// Ensure a pipeline exists for the given playlist, creating one if needed.
    /// (Moved from `mod.rs` to keep that file under the 1000-line cap, #196.)
    pub fn ensure_pipeline(&mut self, playlist_id: i64, ndi_name: &str) {
        self.ensure_pipeline_inner(playlist_id, ndi_name, None);
    }

    /// #196: like [`ensure_pipeline`], but with an optional one-shot the newly
    /// spawned pipeline thread fires (carrying the sender's advertised URL) the
    /// moment its NDI sender is created. `create_startup_senders` passes
    /// `Some(tx)` and awaits it before creating the next output, which
    /// serializes `send_create` in `playlist.id` order for a stable name→port
    /// map. The lazy/runtime path passes `None`. If the pipeline already exists
    /// (closure not run), the sender is dropped and the receiver sees a closed
    /// channel — the caller treats that as "already ready".
    pub(crate) fn ensure_pipeline_inner(
        &mut self,
        playlist_id: i64,
        ndi_name: &str,
        ready_tx: Option<tokio::sync::oneshot::Sender<Option<String>>>,
    ) {
        let event_tx = self.event_tx.clone();

        #[cfg(windows)]
        let ndi_backend = self.ndi_backend.clone();
        #[cfg(not(windows))]
        let ndi_backend: Option<()> = None;

        let genlock_pacing = self.genlock_pacing;
        let ndi_burn_registry = self.ndi_burn_registry.clone();
        let preview_registry = self.preview_registry.clone();
        let ndi_health_registry = self.ndi_health_registry.clone();
        self.pipelines.entry(playlist_id).or_insert_with(|| {
            info!(
                playlist_id,
                ndi_name, genlock_pacing, "creating playback pipeline"
            );
            // #167: count this created pipeline so the heavy-work startup grace
            // knows how many outputs must report before the wall reading is
            // trustworthy (runs once — this closure fires only on a vacant entry).
            ndi_health_registry.register_pipeline();
            // #151: register this output's burn flag (default OFF, never
            // persisted) and hand the shared Arc to the pipeline's submitter.
            let burn_on = ndi_burn_registry.register(ndi_name, genlock_pacing);
            // #15/#178: register the JPEG + A/V-stream taps (lead = #178 A/V-sync, pure fn).
            let lead_ms = crate::playback::preview::preview_stream::lead_ms_for(genlock_pacing);
            let taps = preview_registry.register_taps(playlist_id, lead_ms);
            let pipeline = PlaybackPipeline::spawn(
                ndi_name.to_string(),
                ndi_backend,
                event_tx,
                playlist_id,
                genlock_pacing,
                burn_on,
                taps,
                ready_tx,
            );
            PlaylistPipeline {
                pipeline,
                state: PlayState::Idle,
                mode: PlaybackMode::default(),
                current_video_id: None,
                scene_active: Arc::new(AtomicBool::new(false)),
                title_show_abort: None,
                title_hide_abort: None,
                cached_song: String::new(),
                cached_artist: String::new(),
                cached_duration_ms: 0,
                cached_suppress_en: false,
                cached_lyrics_reference: false,
                last_now_playing_broadcast: None,
                history: VecDeque::with_capacity(PREVIOUS_HISTORY_CAPACITY),
                lyrics_state: None,
                last_presenter_text: None,
                last_resolume_subtitles_signature: None,
                last_lyrics_ws_signature: None,
                cached_position_ms: 0,
                paused_at: None,
            }
        });
    }
}
