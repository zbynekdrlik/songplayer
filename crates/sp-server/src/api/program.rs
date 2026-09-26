//! The program output API (#209, B1 of EPIC #174).
//!
//! - `GET /api/v1/program` — the selected program source, the source before
//!   the latest cut, the cut boundary, and the `SP-program` health counters.
//! - `POST /api/v1/program/cut {"source": <playlist_id>}` — cut the program to
//!   that playlist's output on the boundary after next (frame-accurate, see
//!   `playback::program_bus`). `404` for an unknown playlist. The selection is
//!   persisted first (setting `program_source`) and restored at startup.
//!
//! Both answer the program state plus `vban`, the #210 VBAN audio output's
//! telemetry (`playback::vban_out::VbanStatus`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::AppState;
use crate::playback::program_bus::{ProgramBus, ProgramStatus, persist_selected_source};
use crate::playback::vban_out::VbanStatus;
use crate::playback::wallclock::utc_now_100ns;

/// Body of `POST /api/v1/program/cut`.
#[derive(Debug, Deserialize)]
pub struct CutRequest {
    /// The playlist whose output goes on program.
    pub source: i64,
}

/// The body of both program routes: the program state + the VBAN telemetry.
#[derive(Debug, Serialize)]
pub struct ProgramResponse {
    #[serde(flatten)]
    pub program: ProgramStatus,
    pub vban: VbanStatus,
}

impl ProgramResponse {
    fn new(bus: &ProgramBus, program: ProgramStatus) -> Self {
        Self {
            program,
            vban: bus.vban().status(),
        }
    }
}

/// `GET /api/v1/program`.
pub async fn get_program(State(state): State<AppState>) -> Json<ProgramResponse> {
    let bus = &state.program_bus;
    Json(ProgramResponse::new(bus, bus.status()))
}

/// `POST /api/v1/program/cut` — `200` + the new program state, `404` for an
/// unknown playlist, `500` when the selection cannot be persisted (then nothing
/// is cut).
pub async fn post_program_cut(
    State(state): State<AppState>,
    Json(body): Json<CutRequest>,
) -> Response {
    let exists = sqlx::query("SELECT id FROM playlists WHERE id = ?")
        .bind(body.source)
        .fetch_optional(&state.pool)
        .await;
    match exists {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "unknown playlist").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
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
    Json(ProgramResponse::new(&state.program_bus, status)).into_response()
}

#[cfg(test)]
#[path = "program_tests.rs"]
mod tests;
