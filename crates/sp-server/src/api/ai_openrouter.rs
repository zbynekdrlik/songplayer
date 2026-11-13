//! #321: the OpenRouter text path's routes — the live probe and the blind
//! translation A/B (`ai::openrouter_probe`, `ai::translate_ab`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tracing::{info, warn};

use crate::AppState;
use crate::ai::openrouter_probe::{ProbeRequest, run_probe};
use crate::ai::translate_ab::{self, AbRequest};

/// `POST /api/v1/ai/openrouter/probe {"model"?}`: one tiny completion,
/// always 200 with the report (`ok: false` + why when it failed).
pub async fn probe(
    State(state): State<AppState>,
    body: Option<Json<ProbeRequest>>,
) -> impl IntoResponse {
    // The body is optional (`{"model"?}`): none = the default model.
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let report = run_probe(&state.ai_client, body.model.as_deref()).await;
    if report.ok {
        info!(model = %report.model, latency_ms = report.latency_ms, "openrouter probe: answered");
    } else {
        warn!(model = %report.model, error = ?report.error, "openrouter probe: failed");
    }
    Json(report)
}

/// `GET /api/v1/ai/translate-ab`: the last A/B run's state.
pub async fn ab_status() -> impl IntoResponse {
    Json(translate_ab::status())
}

/// `POST /api/v1/ai/translate-ab {"youtube_ids", "candidates"?}`: starts the
/// blind A/B in the background (202 + its state); 400 for a bad body, 409
/// while paid AI is off or a run holds the slot.
pub async fn ab_start(
    State(state): State<AppState>,
    Json(body): Json<AbRequest>,
) -> axum::response::Response {
    let proxy_model = state.ai_client.settings().model.clone();
    let (ids, candidates) = match translate_ab::checked(body, &proxy_model) {
        Ok(v) => v,
        Err(reason) => return (StatusCode::BAD_REQUEST, reason).into_response(),
    };
    if !crate::paid_ai::enabled(&state.pool).await {
        return (StatusCode::CONFLICT, crate::paid_ai::OFF_REASON).into_response();
    }
    if !translate_ab::begin(ids.len()) {
        return (StatusCode::CONFLICT, "an A/B run is running").into_response();
    }
    // Paid calls on a route anyone on the LAN can reach: the models asked
    // for are logged (each call is bounded, `translate_ab::CALL_MAX_TOKENS`).
    let models: Vec<String> = candidates
        .iter()
        .map(|c| format!("{}:{}", c.provider, c.model))
        .collect();
    info!(songs = ids.len(), models = ?models, "translate A/B: started");
    let seed = translate_ab::random_seed();
    tokio::spawn(run_in_background(state, ids, candidates, seed));
    (StatusCode::ACCEPTED, Json(translate_ab::status())).into_response()
}

/// The background run: progress and the end into the run slot.
#[cfg_attr(test, mutants::skip)] // glue: `translate_ab::run` is tested
async fn run_in_background(
    state: AppState,
    ids: Vec<String>,
    candidates: Vec<translate_ab::Candidate>,
    seed: u64,
) {
    // A panic or a drop of this task leaves the slot `failed`, never `running`.
    let _guard = translate_ab::RunGuard;
    let progress = |done: usize| translate_ab::update(|s| s.songs_done = done);
    let outcome = translate_ab::run(
        &state.ai_client,
        &state.pool,
        &state.cache_dir,
        &ids,
        &candidates,
        seed,
        &progress,
    )
    .await;
    match outcome {
        Ok(dir) => {
            info!(dir = %dir.display(), "translate A/B: done");
            translate_ab::update(|s| {
                s.state = "done".into();
                s.dir = Some(dir.display().to_string());
            });
        }
        Err(e) => {
            warn!(error = %format!("{e:#}"), "translate A/B: failed");
            translate_ab::update(|s| {
                s.state = "failed".into();
                s.error = Some(format!("{e:#}"));
            });
        }
    }
}

#[cfg(test)]
#[path = "ai_openrouter_tests.rs"]
mod tests;
