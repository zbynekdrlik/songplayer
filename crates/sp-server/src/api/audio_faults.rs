//! #257: `GET /api/v1/audio/faults?hours=N` — every audio path's faults
//! over the last `N` hours (1..=2160, default 24), from the persisted
//! ledger (`crate::audio_faults`), so a restart or a deploy shows the same
//! history. The answer's shape is `audio_faults::answer::FaultsAnswer`; an
//! `hours` out of range is a 400 with a fixed text, a store that cannot be
//! read a 500 naming why.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracing::warn;

use crate::AppState;
use crate::audio_faults::answer::{
    FaultsAnswer, MAX_EVENTS, build_answer, parse_hours, window_start,
};
use crate::audio_faults::{probe, store, utc_now_ms};

/// The query, as text: parsed by `parse_hours`, so a refusal never repeats
/// what was sent.
#[derive(Debug, Deserialize)]
pub struct FaultsQuery {
    pub hours: Option<String>,
}

/// The answer over `hours` at `now_ms`.
async fn answer(state: &AppState, hours: u32, now_ms: i64) -> Result<FaultsAnswer, sqlx::Error> {
    let from_ms = window_start(now_ms, hours);
    let totals = store::totals(&state.pool, from_ms).await?;
    let limit = i64::try_from(MAX_EVENTS).unwrap_or(i64::MAX) + 1;
    let newest = store::newest_events(&state.pool, from_ms, limit).await?;
    let details = store::details(&state.pool, from_ms).await?;
    let live = probe::live_paths(&state.program_bus, &state.ndi_health_registry);
    Ok(build_answer(
        now_ms, hours, &totals, newest, &details, &live,
    ))
}

/// `GET /api/v1/audio/faults?hours=N`.
pub async fn get_audio_faults(
    State(state): State<AppState>,
    Query(query): Query<FaultsQuery>,
) -> Response {
    let hours = match parse_hours(query.hours.as_deref()) {
        Ok(hours) => hours,
        Err(refusal) => return (StatusCode::BAD_REQUEST, refusal).into_response(),
    };
    match answer(&state, hours, utc_now_ms()).await {
        Ok(answer) => Json(answer).into_response(),
        Err(e) => {
            warn!(%e, "audio faults: the ledger could not be read");
            let why = format!("the audio fault ledger could not be read: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, why).into_response()
        }
    }
}

#[cfg(test)]
#[path = "audio_faults_tests.rs"]
mod tests;
