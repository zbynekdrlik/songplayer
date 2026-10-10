//! #232: `POST /api/v1/youtube/probe` — the box's yt-dlp resolves one fixed
//! real video as a download would, and downloads nothing
//! (`downloader::probe`, which documents the answer). The post-deploy gate
//! `e2e/post-deploy-youtube.spec.ts` fails the deploy when no format comes
//! back. Takes no body; writes nothing.
//!
//! The cap and the cookie file are read on every call, as a download reads
//! them (the cookie file in the data dir, `cache_dir`'s parent, as
//! `routes_import` finds it).

use axum::Json;
use axum::extract::State;
use tracing::{info, warn};

use crate::AppState;
use crate::downloader::format;
use crate::downloader::probe::{PROBE_VIDEO, YoutubeProbeReport, refused, run};

/// `POST /api/v1/youtube/probe` (module doc). Always 200: a failure is
/// `ok: false` with its `error`.
#[cfg_attr(test, mutants::skip)] // glue around `probe::run`; the route is tested by `probe_route_*`
pub(crate) async fn probe(State(state): State<AppState>) -> Json<YoutubeProbeReport> {
    let cap = format::max_resolution(
        crate::db::models::get_setting(&state.pool, sp_core::config::SETTING_MAX_RESOLUTION)
            .await
            .ok()
            .flatten()
            .as_deref(),
    );
    let cookies_path = state
        .cache_dir
        .parent()
        .unwrap_or(state.cache_dir.as_path())
        .join("cookies.txt");
    let cookies = cookies_path.exists().then_some(cookies_path.as_path());
    let ytdlp = state
        .tool_paths
        .read()
        .await
        .as_ref()
        .map(|t| t.ytdlp.clone());
    let Some(ytdlp) = ytdlp else {
        warn!("youtube probe: yt-dlp is not ready yet");
        return Json(refused(
            PROBE_VIDEO,
            cap,
            cookies.is_some(),
            "yt-dlp is not ready yet on this server".to_string(),
        ));
    };
    let report = run(&ytdlp, cookies, cap).await;
    if report.ok {
        info!(
            youtube_id = %report.youtube_id, cap, cookies = report.cookies,
            format = ?report.format, elapsed_ms = report.elapsed_ms,
            "youtube probe: resolved"
        );
    } else {
        warn!(
            youtube_id = %report.youtube_id, cap, cookies = report.cookies,
            error = ?report.error, elapsed_ms = report.elapsed_ms,
            "youtube probe: failed"
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

    /// The route is wired, takes no body, and with no yt-dlp yet answers
    /// 200 with `ok: false`, the reason, the fixed video and the live cap.
    #[tokio::test]
    async fn probe_route_before_the_tools_are_ready_says_so() {
        let state = test_state().await;
        crate::db::models::set_setting(&state.pool, "max_resolution", "1080")
            .await
            .unwrap();
        let resp = app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/youtube/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(report["ok"], false);
        assert_eq!(report["youtube_id"], "gq-4FVRr_ow");
        assert_eq!(report["cap"], 1080);
        assert_eq!(report["format"], serde_json::Value::Null);
        assert_eq!(report["error"], "yt-dlp is not ready yet on this server");
    }
}
