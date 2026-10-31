//! #256: the dashboard's usage log over HTTP.
//!
//! - `POST /api/v1/ui-usage` takes one batch the dashboard recorded
//!   (`sp_core::ui_usage::parse_batch`: a typed, bounded body, never
//!   through `serde_json::Value`). The body is read as bytes whatever its
//!   content type: `navigator.sendBeacon` sends its string as `text/plain`,
//!   which axum's `Json` would refuse. Each stamp is capped at the
//!   server's clock (a use cannot come after its arrival) and each row
//!   gets the program's source when the batch arrived. A batch that is not
//!   taken is a 400 with a fixed text and nothing written; a stored one is
//!   a 204.
//! - `GET /api/v1/ui-usage/summary?days=N` (1..=400, default 400): over
//!   the last N days up to now, per page, control and kind the count, the
//!   distinct days used, the first and last use and the uses while a
//!   playlist was on air
//!   (`sp_core::ui_usage::UsageSummary`); a `days` out of range is a 400
//!   with a fixed text.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use sp_core::ui_usage::{UsageSummary, parse_batch, parse_days, window_start};
use tracing::{debug, warn};

use crate::AppState;
use crate::db::models_ui_usage;

/// `POST /api/v1/ui-usage`.
pub async fn post_ui_usage(State(state): State<AppState>, body: Bytes) -> Response {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let batch = match parse_batch(&body, now_ms) {
        Ok(batch) => batch,
        Err(refusal) => {
            warn!(
                bytes = body.len(),
                "ui usage: a batch was refused - nothing written"
            );
            return (StatusCode::BAD_REQUEST, refusal).into_response();
        }
    };
    let on_air_source = state.program_bus.on_air_now().source;
    match models_ui_usage::insert_batch(&state.pool, &batch.events, on_air_source).await {
        Ok(()) => {
            debug!(
                events = batch.events.len(),
                ?on_air_source,
                "ui usage: a batch was recorded"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            warn!(%e, "ui usage: a batch could not be stored");
            let why = format!("the ui usage log could not be written: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, why).into_response()
        }
    }
}

/// The query, as text: parsed by `parse_days`, so a refusal never repeats
/// what was sent.
#[derive(Debug, Deserialize)]
pub struct SummaryQuery {
    pub days: Option<String>,
}

/// `GET /api/v1/ui-usage/summary?days=N`.
pub async fn get_ui_usage_summary(
    State(state): State<AppState>,
    Query(query): Query<SummaryQuery>,
) -> Response {
    let days = match parse_days(query.days.as_deref()) {
        Ok(days) => days,
        Err(refusal) => return (StatusCode::BAD_REQUEST, refusal).into_response(),
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let from_ms = window_start(now_ms, days);
    match models_ui_usage::summary_rows(&state.pool, from_ms, now_ms).await {
        Ok(rows) => Json(UsageSummary::from_rows(now_ms, days, rows)).into_response(),
        Err(e) => {
            warn!(%e, "ui usage: the summary could not be read");
            let why = format!("the ui usage log could not be read: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, why).into_response()
        }
    }
}

#[cfg(test)]
#[path = "ui_usage_tests.rs"]
mod tests;
