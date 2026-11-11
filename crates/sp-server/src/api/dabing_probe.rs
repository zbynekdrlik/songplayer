//! #232 unit 2: `POST /api/v1/dabing/probe` — one short real Gemini Live
//! Translate dub from the box, through the dub worker's own child call
//! (`dabing::probe`, which documents the clip and the answer). The post-deploy
//! gate `e2e/post-deploy-dub-live.spec.ts` fails the deploy unless the dub
//! holds sound and a Slovak line. Takes no body; writes nothing to the DB.
//!
//! The model, the voice and the key are read on every call, as the dub worker
//! reads them per job: `dub_model` / `dub_voice` through the worker's own
//! `dub_model_from` / `dub_voice_from`, the key through `paid_ai::gemini_keys`
//! (the first, as the worker sends). 200 once the probe ran (a failure is
//! `ok: false` + `error`), 409 when it was refused before anything was sent.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tracing::{info, warn};

use crate::AppState;
use crate::dabing::probe::{DUB_TIMEOUT, DubProbeReport, ProbeInput, ProbeStatus, run_probe};

/// `POST /api/v1/dabing/probe` (module doc).
#[cfg_attr(test, mutants::skip)] // glue on Gemini Live; `run_probe` is tested, the route by `probe_route_*`
pub async fn probe(State(state): State<AppState>) -> (StatusCode, Json<DubProbeReport>) {
    let setting = |key: &'static str| {
        let pool = state.pool.clone();
        async move {
            crate::db::models::get_setting(&pool, key)
                .await
                .ok()
                .flatten()
        }
    };
    let model = crate::dabing::worker::dub_model_from(
        setting(sp_core::config::SETTING_DUB_MODEL).await.as_deref(),
    );
    let voice = crate::dabing::worker::dub_voice_from(
        setting(sp_core::config::SETTING_DUB_VOICE).await.as_deref(),
    );
    let keys = crate::paid_ai::gemini_keys(&state.pool).await;
    let ffmpeg = state
        .tool_paths
        .read()
        .await
        .as_ref()
        .map(|t| t.ffmpeg.clone());
    let tools_dir = state.cache_dir.join("tools");
    info!(%model, %voice, "dub probe: dubbing one clip through Gemini Live Translate");
    let input = ProbeInput {
        pool: &state.pool,
        cache_dir: &state.cache_dir,
        tools_dir: &tools_dir,
        ffmpeg,
        keys,
        model: &model,
        voice: &voice,
    };
    let (status, report) = run_probe(&input, DUB_TIMEOUT).await;
    if report.ok {
        info!(
            youtube_id = ?report.youtube_id, elapsed_ms = report.elapsed_ms,
            dub_ms = report.dub_ms, dub_rms_dbfs = ?report.dub_rms_dbfs,
            sk_lines = report.sk_lines, sample = %report.sample,
            "dub probe: answered"
        );
    } else {
        warn!(
            youtube_id = ?report.youtube_id, elapsed_ms = report.elapsed_ms,
            refused = (status == ProbeStatus::Refused), error = ?report.error,
            "dub probe: failed"
        );
    }
    let code = match status {
        ProbeStatus::Ran => StatusCode::OK,
        ProbeStatus::Refused => StatusCode::CONFLICT,
    };
    (code, Json(report))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::api::routes::tests::{app, test_state};
    use crate::dabing::probe::DubProbeReport;

    async fn post_probe(state: crate::AppState) -> (StatusCode, DubProbeReport) {
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/dabing/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// The route is wired, takes no body, and with no key refuses with 409
    /// naming the setting; it reports the worker's default model and voice.
    #[tokio::test]
    async fn probe_route_without_a_key_is_refused_naming_the_setting() {
        let (status, report) = post_probe(test_state().await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!report.ok);
        assert_eq!(
            report.error.as_deref(),
            Some("no Gemini API key configured (setting gemini_api_key is empty)")
        );
        assert_eq!(report.model, sp_core::config::DEFAULT_DUB_MODEL);
        assert_eq!(report.voice, sp_core::config::DEFAULT_DUB_VOICE);
        assert_eq!(report.youtube_id, None);
    }

    /// #229 item C: while paid AI is off nothing is sent, a key set or not.
    #[tokio::test]
    async fn probe_route_sends_nothing_while_paid_ai_is_off() {
        let state = test_state().await;
        crate::db::models::set_setting(&state.pool, "gemini_api_key", "k1")
            .await
            .unwrap();
        crate::db::models::set_setting(&state.pool, "paid_ai_enabled", "false")
            .await
            .unwrap();
        let (status, report) = post_probe(state).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(report.error.as_deref(), Some(crate::paid_ai::OFF_REASON));
    }

    /// The model and the voice are the settings the dub worker reads, through
    /// its own parsers (a voice named `Speaker` is the speaker's own voice).
    /// With a key but no venv python the probe is refused naming it.
    #[tokio::test]
    async fn probe_route_reports_the_dub_settings_and_refuses_without_the_venv() {
        let state = test_state().await;
        for (key, value) in [
            ("gemini_api_key", "k1"),
            (
                sp_core::config::SETTING_DUB_MODEL,
                " gemini-9-live-translate ",
            ),
            (sp_core::config::SETTING_DUB_VOICE, "Speaker"),
        ] {
            crate::db::models::set_setting(&state.pool, key, value)
                .await
                .unwrap();
        }
        let (status, report) = post_probe(state).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(report.model, "gemini-9-live-translate");
        assert_eq!(report.voice, "speaker");
        let error = report.error.unwrap();
        assert!(error.starts_with("no venv python at "), "{error}");
        assert!(!error.contains("k1"), "{error}");
    }
}
