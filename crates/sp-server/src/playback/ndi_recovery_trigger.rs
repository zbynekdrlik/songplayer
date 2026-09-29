//! #173: the NDI dark-wall recovery rungs on `PlaybackEngine`: the operator /
//! box-verification trigger for a single rung, and (#221 L4a review) the
//! automatic ladder's rung, both queued on the OBS client by ONE helper.
//!
//! Extracted from `mod.rs` to keep that file under the 1000-line cap. As a child
//! module of `playback`, it reaches the engine's private `pool` + `obs_cmd_tx`.

use tracing::{info, warn};

use super::PlaybackEngine;
use crate::obs::ObsCommand;
use crate::obs::ndi_recovery::RecoveryStep;

/// Why a recovery rung was not queued on the OBS client.
enum RungNotQueued {
    /// No OBS command channel is wired (OBS is not configured).
    NoChannel,
    /// The OBS client's command queue is full or closed (the error's text).
    Queue(String),
}

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
        match self.queue_recovery_rung(&ndi_name, step) {
            Ok(()) => info!(
                playlist_id,
                ndi_name = %ndi_name,
                ?step,
                "ndi-recovery: manual trigger — queued recovery rung over OBS"
            ),
            Err(RungNotQueued::Queue(error)) => warn!(
                playlist_id,
                ndi_name = %ndi_name,
                error = %error,
                "ndi-recovery: manual trigger — failed to queue OBS recovery rung"
            ),
            Err(RungNotQueued::NoChannel) => warn!(
                playlist_id,
                "ndi-recovery: manual trigger — no OBS command channel wired"
            ),
        }
    }

    /// #127 / #173: run the dark-wall ladder's rung `step` for `playlist_id`'s
    /// NDI output `ndi_name` over the healthy OBS WebSocket — the automatic
    /// ladder's side of `handle_health_snapshot` (moved here for that
    /// function's length, #221 L4a review).
    pub(super) fn run_recovery_rung(&self, playlist_id: i64, ndi_name: &str, step: RecoveryStep) {
        match self.queue_recovery_rung(ndi_name, step) {
            Ok(()) => warn!(
                playlist_id,
                ndi_name = %ndi_name,
                ?step,
                "ndi-recovery: dark wall — running recovery rung over OBS"
            ),
            Err(RungNotQueued::Queue(error)) => warn!(
                playlist_id,
                ndi_name = %ndi_name,
                error = %error,
                "ndi-recovery: failed to queue OBS recovery rung"
            ),
            Err(RungNotQueued::NoChannel) => warn!(
                playlist_id,
                ndi_name = %ndi_name,
                "ndi-recovery: dark wall but no OBS command channel wired"
            ),
        }
    }

    /// Queue rung `step` for the NDI output `ndi_name` on the OBS client —
    /// the one send both triggers share. Never blocks: `try_send`.
    fn queue_recovery_rung(&self, ndi_name: &str, step: RecoveryStep) -> Result<(), RungNotQueued> {
        let tx = self.obs_cmd_tx.as_ref().ok_or(RungNotQueued::NoChannel)?;
        let nudge = ObsCommand::NudgeNdiReceiver {
            ndi_name: ndi_name.to_string(),
            step,
        };
        tx.try_send(nudge)
            .map_err(|e| RungNotQueued::Queue(e.to_string()))
    }
}
