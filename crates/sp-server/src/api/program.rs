//! The program output API (#209, B1 of EPIC #174).
//!
//! - `GET /api/v1/program` — the selected program source, the source before
//!   the latest cut, the cut boundary, and the `SP-program` health counters.
//! - `POST /api/v1/program/cut {"source": <playlist_id>}` — cut the program to
//!   that playlist's output on the boundary after next (frame-accurate, see
//!   `playback::program_bus`). `404` for an unknown playlist. The selection is
//!   persisted first (setting `program_source`) and restored at startup.
//!   `{"source": -1}` (`PROGRAM_INPUT_ID`) cuts to the #212 NDI input "OBS
//!   manuál" — accepted only while it is a source (`ndi_input_enabled` with a
//!   non-empty `ndi_input_source`), else `404`.
//!
//! Both answer the program state plus `vban`, the #210 VBAN audio output's
//! telemetry (`playback::vban_out::VbanStatus`), `input`, the #212 NDI
//! input's (`playback::ndi_input::NdiInputStatus`), `remote`, the #213
//! Companion remote control's (`remote::RemoteStatus`), and `follow`, the #215
//! OBS follow (`playback::program_follow::FollowStatus`). The program state
//! itself carries `transition` (#215): the transition the next cut uses, the
//! running window and the transition counters.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use sp_core::config::PROGRAM_INPUT_ID;

use crate::AppState;
use crate::playback::ndi_input::{InputSettings, NdiInputStatus, load_input_settings};
use crate::playback::program_bus::{ProgramBus, ProgramStatus, persist_and_cut};
use crate::playback::program_follow::{FollowSettings, FollowStatus, load_follow_settings};
use crate::playback::vban_out::VbanStatus;
use crate::remote::{RemoteSettings, RemoteStatus, load_remote_settings};

/// Body of `POST /api/v1/program/cut`.
#[derive(Debug, Deserialize)]
pub struct CutRequest {
    /// The playlist whose output goes on program, or `-1` for the NDI input.
    pub source: i64,
}

/// The body of both program routes: the program state + the VBAN, NDI input
/// and remote-control telemetry.
#[derive(Debug, Serialize)]
pub struct ProgramResponse {
    #[serde(flatten)]
    pub program: ProgramStatus,
    pub vban: VbanStatus,
    pub input: NdiInputStatus,
    pub remote: RemoteStatus,
    pub follow: FollowStatus,
}

/// The STORED settings the telemetry blocks report next to their live state.
struct StoredSettings {
    input: InputSettings,
    remote: RemoteSettings,
    follow: FollowSettings,
}

impl ProgramResponse {
    fn new(bus: &ProgramBus, program: ProgramStatus, stored: &StoredSettings) -> Self {
        Self {
            program,
            vban: bus.vban().status(),
            input: bus.input().status(&stored.input),
            remote: bus.remote().status(&stored.remote),
            follow: bus.follow().status(&stored.follow),
        }
    }
}

/// The STORED input, remote-control and follow settings (a save shows at
/// once; the settings tasks apply them within their 5 s poll). An unreadable
/// setting reads as disabled.
async fn stored_settings(state: &AppState) -> StoredSettings {
    let input = load_input_settings(&state.pool).await.unwrap_or_else(|e| {
        warn!(%e, "program: reading the NDI input settings failed");
        InputSettings::default()
    });
    let remote = load_remote_settings(&state.pool).await.unwrap_or_else(|e| {
        warn!(%e, "program: reading the remote-control settings failed");
        RemoteSettings::disabled()
    });
    let follow = load_follow_settings(&state.pool).await.unwrap_or_else(|e| {
        warn!(%e, "program: reading the follow settings failed");
        FollowSettings::default()
    });
    StoredSettings {
        input,
        remote,
        follow,
    }
}

/// `GET /api/v1/program`.
pub async fn get_program(State(state): State<AppState>) -> Json<ProgramResponse> {
    let stored = stored_settings(&state).await;
    let bus = &state.program_bus;
    Json(ProgramResponse::new(bus, bus.status(), &stored))
}

/// `POST /api/v1/program/cut` — `200` + the new program state, `404` for an
/// unknown playlist or an NDI input that is disabled or has no source, `500`
/// when the selection cannot be persisted (then nothing is cut).
pub async fn post_program_cut(
    State(state): State<AppState>,
    Json(body): Json<CutRequest>,
) -> Response {
    let stored = stored_settings(&state).await;
    if body.source == PROGRAM_INPUT_ID {
        if !stored.input.active() {
            return (
                StatusCode::NOT_FOUND,
                "the NDI input is disabled or has no source",
            )
                .into_response();
        }
    } else {
        let exists = sqlx::query("SELECT id FROM playlists WHERE id = ?")
            .bind(body.source)
            .fetch_optional(&state.pool)
            .await;
        match exists {
            Ok(Some(_)) => {}
            Ok(None) => return (StatusCode::NOT_FOUND, "unknown playlist").into_response(),
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        }
    }
    let status = match persist_and_cut(&state.pool, &state.program_bus, body.source).await {
        Ok(status) => status,
        Err(e) => {
            warn!(%e, source = body.source, "program cut: persisting the source failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    info!(
        source = body.source,
        previous = ?status.previous,
        cut_boundary_100ns = ?status.cut_boundary_100ns,
        "program cut"
    );
    Json(ProgramResponse::new(&state.program_bus, status, &stored)).into_response()
}

#[cfg(test)]
#[path = "program_tests.rs"]
mod tests;
