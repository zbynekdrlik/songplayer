//! #132: runtime playlist-pipeline lifecycle for `PlaybackEngine`.
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap.
//!
//! At startup the engine pre-creates a pipeline for every active playlist
//! (`lib.rs::start`). These two methods extend the SAME registration to
//! playlists created / activated / deleted / deactivated at RUNTIME via the
//! API: `EngineCommand::EnsurePipeline` → [`ensure_pipeline_for_playlist`],
//! `EngineCommand::RemovePipeline` → [`remove_pipeline`]. Without them a
//! runtime-created playlist has no pipeline, so putting it on program logs
//! `no pipeline for playlist` until a process restart. #221 L4b: a pipeline
//! created for a playlist already on air goes on program at once (the
//! playback authority's ON came before it existed).
//!
//! [`ensure_pipeline_for_playlist`]: PlaybackEngine::ensure_pipeline_for_playlist
//! [`remove_pipeline`]: PlaybackEngine::remove_pipeline

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, info, warn};

use super::{
    PREVIOUS_HISTORY_CAPACITY, PlayState, PlaybackEngine, PlaybackMode, PlaybackPipeline,
    PlaylistPipeline,
};

impl PlaybackEngine {
    /// #132: Ensure a pipeline exists for a playlist created or activated at
    /// runtime (via the API), so it can play without a process restart; one
    /// already on air goes on program at once (#221 L4b). Reconciles from the
    /// DB — a pipeline is (idempotently) created only when the playlist is
    /// active and has a non-empty NDI output name, mirroring the startup
    /// pre-create loop at startup (`startup_pipelines.rs`), and it starts in
    /// its row's playback mode (#225 unit 2). Delegates to the idempotent
    /// `ensure_pipeline_inner`, so a runtime pipeline gets the same preview
    /// taps and health registration as a boot pipeline. No-op for a missing
    /// row, an inactive playlist, or an empty NDI name.
    pub async fn ensure_pipeline_for_playlist(&mut self, playlist_id: i64) {
        use sqlx::Row;
        let row = match sqlx::query(
            "SELECT name, ndi_output_name, is_active, playback_mode FROM playlists WHERE id = ?",
        )
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

        // #225 unit 2: the pipeline starts in its row's mode.
        let mode = crate::db::models_playlists::row_mode(
            playlist_id,
            &row.get::<String, _>("name"),
            &row.get::<String, _>("playback_mode"),
        );
        info!(
            playlist_id,
            ndi_name, "ensuring pipeline for runtime-created/activated playlist"
        );
        // Idempotent: a no-op if the pipeline already exists.
        self.ensure_pipeline_inner(playlist_id, &ndi_name, mode);
        // #221 L4b: the playback authority's ON for a playlist already on air
        // may have come before its pipeline existed; it goes on program now.
        let on_program = self
            .pipelines
            .get(&playlist_id)
            .is_some_and(|pp| pp.scene_active.load(Ordering::Acquire));
        if self.on_air_contains(playlist_id) && !on_program {
            info!(
                playlist_id,
                "a pipeline of a playlist already on air — on program now"
            );
            self.handle_scene_change(playlist_id, true).await;
        }
    }

    /// #132: Tear down a playlist's pipeline after a runtime delete/deactivate.
    /// Removing it from the map drops the `PlaybackPipeline`, whose `Drop`
    /// sends `Shutdown` to its thread (stopping its paced output) — the same
    /// contract `run()`'s `pipelines.clear()` relies on. Also drops the
    /// playlist's genlock lock-state window. What the dashboard replay last
    /// recorded for it goes too (#225), with or without a pipeline.
    pub fn remove_pipeline(&mut self, playlist_id: i64) {
        // #225: it goes Idle first, told through the one state sender, so the
        // open dashboards agree with a reload's replay (a no-op without a
        // pipeline).
        if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
            pp.state = PlayState::Idle;
        }
        self.broadcast_state(playlist_id);
        // #225 unit 2: a pipeline-less playlist has a record too once its
        // mode changed (`apply_mode`), and it must not outlive the playlist.
        super::dashboard_replay::global().forget(playlist_id);
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

    /// Ensure a pipeline exists for the given playlist, creating one in the
    /// DEFAULT mode if needed: the engine tests' shortcut. Production creates
    /// every pipeline in its row's mode (#225 unit 2), so this one is
    /// test-only.
    #[cfg(test)]
    pub fn ensure_pipeline(&mut self, playlist_id: i64, ndi_name: &str) {
        self.ensure_pipeline_inner(playlist_id, ndi_name, PlaybackMode::default());
    }

    /// Create the playlist's pipeline (idempotent), starting in `mode` (its
    /// row's, #225 unit 2). If the pipeline already exists, its mode is kept.
    /// #221 lane 3: a pipeline has no NDI sender of its own; it feeds the
    /// program bus, so nothing waits for anything here.
    pub(crate) fn ensure_pipeline_inner(
        &mut self,
        playlist_id: i64,
        ndi_name: &str,
        mode: PlaybackMode,
    ) {
        let event_tx = self.event_tx.clone();
        let preview_registry = self.preview_registry.clone();
        let ndi_health_registry = self.ndi_health_registry.clone();
        self.pipelines.entry(playlist_id).or_insert_with(|| {
            info!(playlist_id, ndi_name, "creating playback pipeline");
            // #167: count this created pipeline so the heavy-work startup grace
            // knows how many outputs must report before the wall reading is
            // trustworthy (runs once — this closure fires only on a vacant entry).
            ndi_health_registry.register_pipeline();
            // #15/#178: register the JPEG + A/V-stream taps (the #178 A/V-sync
            // lead of the paced decode seam, a pure fn).
            let lead_ms = crate::playback::preview::preview_stream::decode_seam_lead_ms();
            let taps = preview_registry.register_taps(playlist_id, lead_ms);
            let pipeline =
                PlaybackPipeline::spawn(ndi_name.to_string(), event_tx, playlist_id, taps);
            PlaylistPipeline {
                pipeline,
                state: PlayState::Idle,
                mode,
                current_video_id: None,
                scene_active: Arc::new(AtomicBool::new(false)),
                title_show_abort: None,
                title_hide_abort: None,
                scene_off_due: None,
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
                title_clock: None,
                play_start_ms: 0,
                paused_at: None,
            }
        });
    }
}
