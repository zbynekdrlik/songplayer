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

use tracing::{debug, info, warn};

use super::PlaybackEngine;

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
}
