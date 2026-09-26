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
//! telemetry (`playback::vban_out::VbanStatus`), and `input`, the #212 NDI
//! input's (`playback::ndi_input::NdiInputStatus`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use sp_core::config::PROGRAM_INPUT_ID;

use crate::AppState;
use crate::playback::ndi_input::{InputSettings, NdiInputStatus, load_input_settings};
use crate::playback::program_bus::{ProgramBus, ProgramStatus, persist_selected_source};
use crate::playback::vban_out::VbanStatus;
use crate::playback::wallclock::utc_now_100ns;

/// Body of `POST /api/v1/program/cut`.
#[derive(Debug, Deserialize)]
pub struct CutRequest {
    /// The playlist whose output goes on program, or `-1` for the NDI input.
    pub source: i64,
}

/// The body of both program routes: the program state + the VBAN and NDI
/// input telemetry.
#[derive(Debug, Serialize)]
pub struct ProgramResponse {
    #[serde(flatten)]
    pub program: ProgramStatus,
    pub vban: VbanStatus,
    pub input: NdiInputStatus,
}

impl ProgramResponse {
    fn new(bus: &ProgramBus, program: ProgramStatus, input: &InputSettings) -> Self {
        Self {
            program,
            vban: bus.vban().status(),
            input: bus.input().status(input),
        }
    }
}

/// The STORED input settings (a save shows at once; the input thread applies
/// them within its 5 s poll). An unreadable setting reads as disabled.
async fn stored_input_settings(state: &AppState) -> InputSettings {
    load_input_settings(&state.pool).await.unwrap_or_else(|e| {
        warn!(%e, "program: reading the NDI input settings failed");
        InputSettings::default()
    })
}

/// `GET /api/v1/program`.
pub async fn get_program(State(state): State<AppState>) -> Json<ProgramResponse> {
    let input = stored_input_settings(&state).await;
    let bus = &state.program_bus;
    Json(ProgramResponse::new(bus, bus.status(), &input))
}

/// `POST /api/v1/program/cut` — `200` + the new program state, `404` for an
/// unknown playlist or an NDI input that is disabled or has no source, `500`
/// when the selection cannot be persisted (then nothing is cut).
pub async fn post_program_cut(
    State(state): State<AppState>,
    Json(body): Json<CutRequest>,
) -> Response {
    let input = stored_input_settings(&state).await;
    if body.source == PROGRAM_INPUT_ID {
        if !input.active() {
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
    if let Err(e) = persist_selected_source(&state.pool, body.source).await {
        warn!(%e, source = body.source, "program cut: persisting the source failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let status = state.program_bus.cut(body.source, utc_now_100ns());
    info!(
        source = body.source,
        previous = ?status.previous,
        cut_boundary_100ns = ?status.cut_boundary_100ns,
        "program cut"
    );
    Json(ProgramResponse::new(&state.program_bus, status, &input)).into_response()
}

#[cfg(test)]
#[path = "program_tests.rs"]
mod tests;
