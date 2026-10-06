//! #144: `POST /api/v1/lyrics/g35t/probe` — one short real Gemini 3.5
//! Transcribe request from the box, through the worker's own call
//! (`lyrics::g35t_probe`, which documents the clip and the answer). The
//! post-deploy gate `e2e/post-deploy-g35t.spec.ts` fails the deploy when it
//! does not answer with words. Takes no body; writes nothing to the DB.
//!
//! The key list is read from the setting on every call, as the lyrics worker
//! reads it per song, so the probe sees a key change without a restart.

use axum::Json;
use axum::extract::State;
use tracing::{info, warn};

use crate::AppState;
use crate::gemini_api::{GEMINI_API_ROOT, gemini_keys_from_setting};
use crate::lyrics::g35t_probe::{G35tProbeReport, run_probe};

/// `POST /api/v1/lyrics/g35t/probe` (module doc). Always 200: a failure is
/// `ok: false` with its `error`.
#[cfg_attr(test, mutants::skip)] // glue on Google's root; `run_probe` is tested, the route by `probe_route_*`
pub async fn probe(State(state): State<AppState>) -> Json<G35tProbeReport> {
    let csv = crate::db::models::get_setting(&state.pool, "gemini_api_key")
        .await
        .inspect_err(|e| warn!("g35t probe: reading gemini_api_key failed: {e}"))
        .ok()
        .flatten()
        .unwrap_or_default();
    let keys = gemini_keys_from_setting(&csv);
    let ffmpeg = state
        .tool_paths
        .read()
        .await
        .as_ref()
        .map(|t| t.ffmpeg.clone());
    info!(
        keys = keys.len(),
        "g35t probe: sending one clip to Gemini 3.5 Transcribe"
    );
    let client = reqwest::Client::new();
    let report = run_probe(
        &state.pool,
        &state.cache_dir,
        ffmpeg,
        &keys,
        &client,
        GEMINI_API_ROOT,
    )
    .await;
    if report.ok {
        info!(
            key_index = ?report.key_index, word_count = report.word_count,
            latency_ms = report.latency_ms, clip = ?report.clip, sample = %report.sample,
            "g35t probe: answered"
        );
    } else {
        warn!(
            key_index = ?report.key_index, word_count = report.word_count,
            latency_ms = report.latency_ms, clip = ?report.clip, error = ?report.error,
            "g35t probe: failed"
        );
    }
    Json(report)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::api::routes::tests::{app, test_state};
    use crate::lyrics::g35t_probe::G35tProbeReport;

    /// The route is wired, takes no body and answers the report: with no
    /// key configured it stops before anything is sent, naming the setting,
    /// and still reports the model and the hint the request would carry.
    #[tokio::test]
    async fn probe_route_without_a_key_reports_the_setting_and_the_request_shape() {
        let state = test_state().await;
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/lyrics/g35t/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let report: G35tProbeReport = serde_json::from_slice(&body).unwrap();
        assert!(!report.ok);
        assert_eq!(
            report.error.as_deref(),
            Some("no Gemini API key configured (setting gemini_api_key is empty)")
        );
        assert_eq!(report.model, "gemini-3.5-transcribe");
        assert_eq!(report.language_codes, ["en-US", "es-419"]);
        assert_eq!(report.key_index, None);
        assert_eq!(report.word_count, 0);
        assert_eq!(report.clip, None);
    }
}
