//! #173: admin/operator endpoint to fire ONE NDI dark-wall recovery rung for a
//! playlist over the healthy OBS WebSocket. Lives in its own file so
//! `api/routes.rs` stays under the 1000-line cap.
//!
//! `POST /api/v1/ndi/recover/{playlist_id}?step=clear|toggle|recreate` forwards
//! `EngineCommand::TriggerNdiRecovery`; the engine resolves the playlist's NDI
//! output name and sends `ObsCommand::NudgeNdiReceiver`. This is the honest box
//! verification for rung 2 (`recreate`) — a wrong `ndi_source_name` would make
//! the ladder's own `NoMatch` fire, so the recreate path is exercised directly —
//! and a genuine operator remedy. It does NOT alter the automatic ladder state.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;

use crate::obs::ndi_recovery::RecoveryStep;
use crate::{AppState, EngineCommand};

/// Query params for `POST /api/v1/ndi/recover/{playlist_id}`.
#[derive(Debug, Deserialize)]
pub struct NdiRecoverQuery {
    /// Which rung to fire: `clear` | `toggle` | `recreate`.
    pub step: String,
}

/// Parse the `step` query value into a recovery rung. Pure so it is unit-tested
/// (this file IS mutation-scored — `obs/` is excluded, `api/` is not).
fn parse_recovery_step(step: &str) -> Option<RecoveryStep> {
    match step {
        "clear" | "clear_restore" | "clearrestore" => Some(RecoveryStep::ClearRestore),
        "toggle" | "toggle_scene_item" => Some(RecoveryStep::ToggleSceneItem),
        "recreate" | "recreate_input" => Some(RecoveryStep::RecreateInput),
        _ => None,
    }
}

/// POST /api/v1/ndi/recover/{playlist_id}?step=clear|toggle|recreate — fire one
/// dark-wall recovery rung. `202 Accepted` on a known step, `400` on an unknown
/// one. The work runs asynchronously on the engine (like the automatic ladder).
// mutants::skip: I/O handler — reads `step`, forwards an EngineCommand; the only
// testable logic (`parse_recovery_step`) is a separate pure fn with its own
// tests, and the forward is exercised on the box.
#[cfg_attr(test, mutants::skip)]
pub async fn post_ndi_recover(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
    Query(q): Query<NdiRecoverQuery>,
) -> impl IntoResponse {
    let step = match parse_recovery_step(&q.step) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "unknown step (use clear|toggle|recreate)",
            )
                .into_response();
        }
    };
    // Guaranteed delivery (`.send().await`) like the other engine commands.
    let _ = state
        .engine_tx
        .send(EngineCommand::TriggerNdiRecovery { playlist_id, step })
        .await;
    StatusCode::ACCEPTED.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recovery_step_maps_each_rung() {
        assert_eq!(
            parse_recovery_step("clear"),
            Some(RecoveryStep::ClearRestore)
        );
        assert_eq!(
            parse_recovery_step("clear_restore"),
            Some(RecoveryStep::ClearRestore)
        );
        assert_eq!(
            parse_recovery_step("toggle"),
            Some(RecoveryStep::ToggleSceneItem)
        );
        assert_eq!(
            parse_recovery_step("recreate"),
            Some(RecoveryStep::RecreateInput)
        );
        assert_eq!(
            parse_recovery_step("recreate_input"),
            Some(RecoveryStep::RecreateInput)
        );
    }

    #[test]
    fn parse_recovery_step_rejects_unknown() {
        assert_eq!(parse_recovery_step(""), None);
        assert_eq!(parse_recovery_step("nope"), None);
        assert_eq!(parse_recovery_step("CLEAR"), None); // case-sensitive by design
    }
}
