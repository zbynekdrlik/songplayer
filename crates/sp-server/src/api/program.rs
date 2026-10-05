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
//!   #221 L4a: the cut goes through the ONE switch path
//!   (`program_switch::switch_source`, `via=dashboard`): under the bus's
//!   `switch_order`, recorded as `remote.last_remote_cut`, a playlist cut
//!   first and then mirrored to cg OBS (until B4 step 6), -1 a cut only.
//!
//! Both answer the program state plus `vban`, the #210 VBAN audio output's
//! telemetry (`playback::vban_out::VbanStatus`), `input`, the #212 NDI
//! input's (`playback::ndi_input::NdiInputStatus`), `remote`, the #213
//! Companion remote control's (`remote::RemoteStatus`, #221 L3: with
//! `program_scene`, SP-program's scene name), and `follow`, the #215
//! OBS follow (`playback::program_follow::FollowStatus`). The program state
//! itself carries `transition` (#215): the transition the next cut uses, the
//! running window and the transition counters. #221 L4a: `legacy_cg`, what
//! SongPlayer told cg OBS to show (`playback::legacy_cg::LegacyCgStatus`).
//! #223 S2: `max`, the `SP-program-MAX` output (`playback::program_max::MaxStatus`:
//! the setting, the `program-max` thread's state, the counters and the GPU /
//! Spout cost p99s).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use sp_core::config::PROGRAM_INPUT_ID;

use crate::AppState;
use crate::playback::legacy_cg::LegacyCgStatus;
use crate::playback::ndi_input::{InputSettings, NdiInputStatus, load_input_settings};
use crate::playback::program_bus::{ProgramBus, ProgramStatus};
use crate::playback::program_follow::{FollowSettings, FollowStatus, load_follow_settings};
use crate::playback::program_max::MaxStatus;
use crate::playback::program_switch::{SwitchCtx, Via, switch_source};
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
    pub legacy_cg: LegacyCgStatus,
    pub max: MaxStatus,
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
            remote: bus.remote().status(&stored.remote, &bus.on_air_now()),
            follow: bus.follow().status(&stored.follow),
            legacy_cg: bus.legacy_cg().status(),
            max: bus.max().status(),
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
/// when the playlists cannot be read or the selection cannot be persisted
/// (then nothing is cut). #221 L4a: through the one switch path
/// (`switch_source`, `via=dashboard`), which mirrors a playlist to cg OBS.
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
    let bus = &state.program_bus;
    let upstream = bus.legacy_cg().link();
    let ctx = SwitchCtx {
        pool: &state.pool,
        bus,
        upstream: &upstream,
    };
    let status = match switch_source(&ctx, body.source, Via::Dashboard).await {
        Ok(status) => status,
        Err(e) => {
            warn!(%e, source = body.source, "program cut: nothing was cut");
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
#[cfg(test)]
#[path = "program_tests_max.rs"]
mod tests_max;
#[cfg(test)]
#[path = "program_tests_switch.rs"]
mod tests_switch;
