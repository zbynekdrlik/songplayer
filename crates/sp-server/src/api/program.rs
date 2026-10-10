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
//!   non-empty `ndi_input_source`), else `404`. `{"source": -2}`
//!   (`PROGRAM_BLANK_ID`, #245) cuts to Blank, SongPlayer's own black:
//!   always accepted.
//!   #221 L4a: the cut goes through the ONE switch path
//!   (`program_switch::switch_source`, `via=dashboard`): under the bus's
//!   `switch_order`, recorded as `remote.last_remote_cut`. #221 B4 step 6:
//!   cg OBS is told nothing (the legacy mirror is deleted). #221
//!   ROZHODNUTÉ 6022247729: a playlist that is inactive or whose catalog
//!   names no scene is refused with `409` + `{reason, error}` (every
//!   consumer takes SP-program, so the cut would black them all), recorded
//!   as a keep.
//!
//! Both answer the program state plus `outputs` (#233): every audio output
//! of the list in list order (`playback::audio_out::OutputStatus`: id, type,
//! name, enabled, state + reason, rate, format, channels, delay, latency,
//! blocks sent / dropped, and a VBAN entry's #210 telemetry under `vban` —
//! the top-level `vban` block is gone), `audio_network_rate` and
//! `outputs_problems` (stored entries this version could not run); `input`,
//! the #212 NDI input's (`playback::ndi_input::NdiInputStatus`), `remote`, the #213
//! Companion remote control's (`remote::RemoteStatus`, #221 L3: with
//! `program_scene`, SP-program's scene name), and `degraded_reason` (#221
//! B4 step 6: "no NDI receiver on SP-program" while a source is on program
//! and nothing receives `SP-program`, `ndi_health_expect`). The program
//! state itself carries `transition` (#215): the transition the next cut
//! uses, the running window and the transition counters. #223 S2: `max`, the
//! `SP-program-MAX` output (`playback::program_max::MaxStatus`: the setting,
//! the `program-max` thread's state, the counters and the GPU / Spout cost
//! p99s). #221 L5 deleted the `follow` block (the OBS follow) and B4 step 6
//! the `legacy_cg` record. `cut_refused` (ROZHODNUTÉ 6022247729) lists the
//! playlists a cut refuses now, `{source, reason}` in id order
//! (`program_switch::refused_sources`, the cut's own rule; `null` when the
//! playlists cannot be read): the dashboard disables their buttons.
//!
//! #228: both answers also carry `burn_on` (the 911014 burn switch, default
//! off, never persisted), `burned_boundaries` (since the start) and
//! `on_air_item`: the item the `SP-program` sender put on the wire last
//! (`playback::program_item::ItemStatus`: playlist, video, `started_at_utc_ns`,
//! `position_ms`, `frame`, `frame_utc_ns`) with the video's `youtube_id` and
//! `title` from the store, `null` while no item frame of the source on
//! program went out. `POST /api/v1/program/burn {"on": bool}` turns the burn
//! on or off: `200 {"burn_on": bool}`.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use sp_core::config::{PROGRAM_BLANK_ID, PROGRAM_INPUT_ID};

use crate::AppState;
use crate::playback::audio_out::OutputStatus;
use crate::playback::ndi_health_expect::program_degraded_reason;
use crate::playback::ndi_input::{InputSettings, NdiInputStatus, load_input_settings};
use crate::playback::program_bus::{ProgramBus, ProgramStatus};
use crate::playback::program_item::ItemStatus;
use crate::playback::program_max::MaxStatus;
use crate::playback::program_switch::{
    RefusedSource, SourceError, Via, refused_sources, switch_source,
};
use crate::playback::program_trace::TraceAnswer;
use crate::playback::wallclock::utc_now_100ns;
use crate::remote::{RemoteSettings, RemoteStatus, load_remote_settings};

/// Body of `POST /api/v1/program/cut`.
#[derive(Debug, Deserialize)]
pub struct CutRequest {
    /// The playlist whose output goes on program, or `-1` for the NDI input.
    pub source: i64,
}

/// The `409` body of a refused cut (#221 ROZHODNUTÉ 6022247729): the reason
/// code (`sp_core::program_refusal`, which the dashboard turns into its
/// Slovak text) and the reason in words.
#[derive(Debug, Serialize)]
pub struct CutRefusedBody {
    pub reason: &'static str,
    pub error: &'static str,
}

/// The body of both program routes: the program state + the audio outputs,
/// NDI input and remote-control telemetry, and SP-program's receiver
/// expectation.
#[derive(Debug, Serialize)]
pub struct ProgramResponse {
    #[serde(flatten)]
    pub program: ProgramStatus,
    /// #221 B4 step 6: SP-program's degraded reason — a source is on
    /// program and nothing receives `SP-program` (`null` otherwise, and
    /// before the sender polled its receivers).
    pub degraded_reason: Option<&'static str>,
    /// #233: every audio output, in list order.
    pub outputs: Vec<OutputStatus>,
    /// #233: the network sample rate an output at "network" runs at.
    pub audio_network_rate: u32,
    /// #233: stored entries this version could not run (named, skipped).
    pub outputs_problems: Vec<String>,
    pub input: NdiInputStatus,
    pub remote: RemoteStatus,
    pub max: MaxStatus,
    /// #221 ROZHODNUTÉ 6022247729: the playlists a cut refuses now (`null`
    /// when the playlists cannot be read).
    pub cut_refused: Option<Vec<RefusedSource>>,
    /// #228: the 911014 burn switch, and the boundaries burned since start.
    pub burn_on: bool,
    pub burned_boundaries: u64,
    /// #228: the item on the wire (`null`: none).
    pub on_air_item: Option<OnAirItem>,
}

/// #228: the item on `SP-program`, as the sender last put it on the wire,
/// with its video's YouTube id and title from the store (`null` when the
/// row cannot be read).
#[derive(Debug, Serialize)]
pub struct OnAirItem {
    #[serde(flatten)]
    pub item: ItemStatus,
    pub youtube_id: Option<String>,
    pub title: Option<String>,
}

/// The body of `POST /api/v1/program/burn`.
#[derive(Debug, Deserialize)]
pub struct BurnRequest {
    pub on: bool,
}

/// The answer of `POST /api/v1/program/burn`: the switch as it is now.
#[derive(Debug, Serialize)]
pub struct BurnAnswer {
    pub burn_on: bool,
}

/// The STORED settings the telemetry blocks report next to their live state.
struct StoredSettings {
    input: InputSettings,
    remote: RemoteSettings,
}

impl ProgramResponse {
    fn new(
        bus: &ProgramBus,
        program: ProgramStatus,
        stored: &StoredSettings,
        cut_refused: Option<Vec<RefusedSource>>,
        on_air_item: Option<OnAirItem>,
    ) -> Self {
        let health = &program.health;
        let polled = health.receivers_polled.then_some(health.connections);
        let degraded_reason = program_degraded_reason(program.source, polled);
        Self {
            program,
            degraded_reason,
            outputs: bus.outputs().status(),
            audio_network_rate: bus.outputs().network_rate(),
            outputs_problems: bus.outputs().problems(),
            input: bus.input().status(&stored.input),
            remote: bus.remote().status(&stored.remote, &bus.on_air_now()),
            max: bus.max().status(),
            cut_refused,
            burn_on: bus.item().burn_on(),
            burned_boundaries: bus.item().burned(),
            on_air_item,
        }
    }
}

/// #228: the item on the wire, with its video's YouTube id and title (both
/// `null` when the row cannot be read: WARNed).
async fn on_air_item(state: &AppState) -> Option<OnAirItem> {
    let item = state.program_bus.item().on_air()?;
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT youtube_id, title FROM videos WHERE id = ?",
    )
    .bind(item.video_id)
    .fetch_optional(&state.pool)
    .await
    .inspect_err(
        |e| warn!(%e, video_id = item.video_id, "program: reading the on-air item's video failed"),
    )
    .ok()
    .flatten();
    let (youtube_id, title) = row.map_or((None, None), |(id, title)| (Some(id), title));
    Some(OnAirItem {
        item,
        youtube_id,
        title,
    })
}

/// The playlists a cut refuses now; `None` (WARN) when they cannot be read.
async fn cut_refused(state: &AppState) -> Option<Vec<RefusedSource>> {
    refused_sources(&state.pool)
        .await
        .inspect_err(|e| warn!(%e, "program: reading the playlists a cut refuses failed"))
        .ok()
}

/// The STORED input and remote-control settings (a save shows at once; the
/// settings tasks apply them within their 5 s poll). An unreadable setting
/// reads as disabled.
async fn stored_settings(state: &AppState) -> StoredSettings {
    let input = load_input_settings(&state.pool).await.unwrap_or_else(|e| {
        warn!(%e, "program: reading the NDI input settings failed");
        InputSettings::default()
    });
    let remote = load_remote_settings(&state.pool).await.unwrap_or_else(|e| {
        warn!(%e, "program: reading the remote-control settings failed");
        RemoteSettings::disabled()
    });
    StoredSettings { input, remote }
}

/// `GET /api/v1/program`.
pub async fn get_program(State(state): State<AppState>) -> Json<ProgramResponse> {
    let stored = stored_settings(&state).await;
    let refused = cut_refused(&state).await;
    let item = on_air_item(&state).await;
    let bus = &state.program_bus;
    Json(ProgramResponse::new(
        bus,
        bus.status(),
        &stored,
        refused,
        item,
    ))
}

/// `POST /api/v1/program/burn {"on": bool}` (#228): turn the 911014 burn on
/// `SP-program` on or off — in memory only, a restart starts it off. `200`
/// with the switch as it is now.
pub async fn post_program_burn(
    State(state): State<AppState>,
    Json(body): Json<BurnRequest>,
) -> Json<BurnAnswer> {
    let item = state.program_bus.item();
    item.set_burn(body.on);
    info!(
        on = body.on,
        "program: the 911014 burn on SP-program switched"
    );
    Json(BurnAnswer {
        burn_on: item.burn_on(),
    })
}

/// `POST /api/v1/program/cut` — `200` + the new program state, `404` for an
/// unknown playlist or an NDI input that is disabled or has no source, `409`
/// for a playlist that is inactive or whose catalog names no scene (#221
/// ROZHODNUTÉ 6022247729), `500` when the playlists cannot be read or the
/// selection cannot be persisted (then nothing is cut). #221 L4a: through
/// the one switch path (`switch_source`, `via=dashboard`).
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
    } else if body.source != PROGRAM_BLANK_ID {
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
    let status = match switch_source(&state.pool, bus, body.source, Via::Dashboard).await {
        Ok(status) => status,
        Err(SourceError::Refused(refusal)) => {
            let body = CutRefusedBody {
                reason: refusal.reason(),
                error: refusal.message(),
            };
            return (StatusCode::CONFLICT, Json(body)).into_response();
        }
        Err(SourceError::Store(e)) => {
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
    let refused = cut_refused(&state).await;
    let item = on_air_item(&state).await;
    Json(ProgramResponse::new(bus, status, &stored, refused, item)).into_response()
}

/// The query of `GET /api/v1/program/trace` (#147), as text: parsed by
/// `TraceAnswer::build`, so a refusal never repeats what was sent.
#[derive(Debug, Deserialize)]
pub struct TraceQuery {
    pub from_utc_ms: Option<String>,
    pub to_utc_ms: Option<String>,
}

/// `GET /api/v1/program/trace?from_utc_ms=&to_utc_ms=` (#147): the
/// `SP-program` sender's per-boundary records whose submit returned in the
/// window (`playback::program_trace::TraceAnswer`, compact rows). `to`
/// defaults to now, `from` to 2 min before `to`; a window over 2 min ends 2
/// min after `from` (`clamped`). `400` with a fixed text for a value that is
/// not an integer, or a `from` after its `to`. Reads a snapshot of the ring:
/// never waits for the sender.
pub async fn get_program_trace(
    State(state): State<AppState>,
    Query(query): Query<TraceQuery>,
) -> Response {
    let now_ms = utc_now_100ns().div_euclid(10_000);
    let from = query.from_utc_ms.as_deref();
    let to = query.to_utc_ms.as_deref();
    match TraceAnswer::build(state.program_bus.trace(), from, to, now_ms) {
        Ok(answer) => Json(answer).into_response(),
        Err(refusal) => (StatusCode::BAD_REQUEST, refusal).into_response(),
    }
}

#[cfg(test)]
#[path = "program_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "program_tests_burn.rs"]
mod tests_burn;
#[cfg(test)]
#[path = "program_tests_max.rs"]
mod tests_max;
#[cfg(test)]
#[path = "program_tests_outputs.rs"]
mod tests_outputs;
#[cfg(test)]
#[path = "program_tests_switch.rs"]
mod tests_switch;
#[cfg(test)]
#[path = "program_tests_trace.rs"]
mod tests_trace;
