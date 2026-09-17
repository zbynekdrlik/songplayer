//! #173: operator / box-verification trigger for a single NDI dark-wall recovery
//! rung on `PlaybackEngine`.
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap. As a child
//! module of `playback`, it reaches the engine's private `pool` + `obs_cmd_tx`.

use tracing::{info, warn};

use super::PlaybackEngine;
use crate::obs::ObsCommand;
use crate::obs::ndi_recovery::RecoveryStep;

impl PlaybackEngine {
    /// Fire a single NDI dark-wall recovery rung for `playlist_id` over the
    /// healthy OBS WebSocket — the operator / box-verification trigger behind
    /// `POST /api/v1/ndi/recover/{playlist_id}?step=...`. Resolves the playlist's
    /// `ndi_output_name` and forwards `ObsCommand::NudgeNdiReceiver`; it does NOT
    /// touch the automatic ladder's per-pipeline state.
    // mutants::skip: I/O plumbing — a DB read + a channel send — with no logic
    // branch worth a killable mutant; exercised on the box (E2E test 12 + the
    // admin endpoint) since it needs a live OBS WebSocket.
    #[cfg_attr(test, mutants::skip)]
    pub async fn trigger_ndi_recovery(&self, playlist_id: i64, step: RecoveryStep) {
        let ndi_name: Option<String> =
            sqlx::query_scalar("SELECT ndi_output_name FROM playlists WHERE id = ?")
                .bind(playlist_id)
                .fetch_optional(&self.pool)
                .await
                .unwrap_or_default()
                .filter(|s: &String| !s.is_empty());
        let ndi_name = match ndi_name {
            Some(n) => n,
            None => {
                warn!(
                    playlist_id,
                    "ndi-recovery: manual trigger — playlist has no NDI output name"
                );
                return;
            }
        };
        match self.obs_cmd_tx.as_ref() {
            Some(tx) => match tx.try_send(ObsCommand::NudgeNdiReceiver {
                ndi_name: ndi_name.clone(),
                step,
            }) {
                Ok(()) => info!(
                    playlist_id,
                    ndi_name = %ndi_name,
                    ?step,
                    "ndi-recovery: manual trigger — queued recovery rung over OBS"
                ),
                Err(e) => warn!(
                    playlist_id,
                    ndi_name = %ndi_name,
                    error = %e,
                    "ndi-recovery: manual trigger — failed to queue OBS recovery rung"
                ),
            },
            None => warn!(
                playlist_id,
                "ndi-recovery: manual trigger — no OBS command channel wired"
            ),
        }
    }
}
