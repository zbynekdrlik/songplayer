//! Karaoke live-control application for `PlaybackEngine` (#14).
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap. As a child
//! module of `playback`, it can access the engine's private fields.

use sp_core::playback::KaraokeMode;
use sp_core::ws::ServerMsg;
use tracing::{info, warn};

use super::PlaybackEngine;
use super::pipeline::PipelineCommand;
use super::state::PlayState;

impl PlaybackEngine {
    /// Apply a new karaoke mode + vocal gain. Updates the process-global live
    /// control, persists both to settings (restored on restart), broadcasts the
    /// new state, and — when the MODE changed — reloads every playing pipeline at
    /// its current position so the wall reflects it immediately (the vocal-gain
    /// slider is already live per audio chunk, so a gain-only change needs no
    /// reload).
    #[cfg_attr(test, mutants::skip)]
    pub async fn set_karaoke(&mut self, mode: KaraokeMode, vocal_gain: f32) {
        let control = crate::stems::control::global();
        let old_mode = control.mode();
        control.set_mode(mode);
        control.set_vocal_gain(vocal_gain);

        // Persist so the operator's choice survives a restart.
        let _ = crate::db::models::set_setting(&self.pool, "karaoke_mode", mode.as_str()).await;
        let _ = crate::db::models::set_setting(
            &self.pool,
            "karaoke_vocal_gain",
            &control.vocal_gain().to_string(),
        )
        .await;

        // Broadcast the new live state to the dashboard.
        let _ = self.ws_event_tx.send(ServerMsg::KaraokeStateChanged {
            mode,
            vocal_gain: control.vocal_gain(),
        });

        if mode == old_mode {
            return; // gain-only change is already live via the shared atomic
        }

        // A mode change swaps which files the decoder opens → reload each playing
        // pipeline at its current position (atomic play-from-position, #88).
        let reloads: Vec<(i64, i64, u64)> = self
            .pipelines
            .iter()
            .filter_map(|(pid, pp)| match pp.state {
                PlayState::Playing { video_id } => Some((*pid, video_id, pp.cached_position_ms)),
                _ => None,
            })
            .collect();
        for (playlist_id, video_id, position_ms) in reloads {
            match crate::db::models::get_song_paths(&self.pool, video_id).await {
                Ok(Some((video_path, audio_path))) => {
                    if let Some(pp) = self.pipelines.get_mut(&playlist_id) {
                        info!(
                            playlist_id,
                            video_id,
                            position_ms,
                            ?mode,
                            "karaoke mode change → reloading song at position"
                        );
                        pp.pipeline.send(PipelineCommand::Play {
                            video: video_path.into(),
                            audio: audio_path.into(),
                            start_position_ms: Some(position_ms),
                        });
                    }
                }
                Ok(None) => {}
                Err(e) => warn!(playlist_id, video_id, %e, "karaoke reload: path lookup failed"),
            }
        }
    }
}
