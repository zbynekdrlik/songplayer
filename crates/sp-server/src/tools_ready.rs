//! The startup tools task once `ensure_tools` has the tools on disk (#144):
//! publish them — the `ToolsStatus` fields, the `ToolsStatus` event, the
//! `tool_paths` — then run the follow-ups the caller hands in (the yt-dlp
//! self-update #140, the sample-rate sweep #40, the startup sync and every
//! worker spawn).

use std::future::Future;
use std::sync::Arc;

use sp_core::ws::ServerMsg;
use tokio::sync::{RwLock, broadcast};
use tracing::info;

use crate::ToolsStatus;
use crate::downloader::tools::ToolPaths;

/// What the startup probes found once the tools are on disk.
pub(crate) struct ToolsFound {
    pub ytdlp_version: Option<String>,
    /// yt-dlp has a working JS runtime (Deno) for YouTube's n-challenge (#189).
    pub js_runtime_ok: bool,
    /// Bundled Deno version, when present.
    pub deno_version: Option<String>,
}

/// Where the ready tools are published: the status `GET /api/v1/status` and
/// a new dashboard socket read, the event the open dashboards get, and the
/// paths the playlist sync handler reads.
pub(crate) struct ToolsSinks {
    pub status: Arc<RwLock<ToolsStatus>>,
    pub paths: Arc<RwLock<Option<ToolPaths>>>,
    pub events: broadcast::Sender<ServerMsg>,
}

/// Publish the ready tools into `sinks`, then run `follow_ups` and return
/// what it returns.
pub(crate) async fn publish_then<R>(
    sinks: ToolsSinks,
    paths: ToolPaths,
    found: ToolsFound,
    follow_ups: impl Future<Output = R>,
) -> R {
    let mut ts = sinks.status.write().await;
    ts.ytdlp_available = true;
    ts.ffmpeg_available = true;
    ts.ytdlp_version = found.ytdlp_version.clone();
    ts.js_runtime_ok = found.js_runtime_ok;
    ts.deno_version = found.deno_version.clone();
    let _ = sinks.events.send(ServerMsg::ToolsStatus {
        ytdlp_available: true,
        ffmpeg_available: true,
        ytdlp_version: found.ytdlp_version,
        js_runtime_ok: found.js_runtime_ok,
        deno_version: found.deno_version,
    });
    *sinks.paths.write().await = Some(paths);
    info!("tools ready: yt-dlp and FFmpeg available");
    follow_ups.await
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sp_core::ws::ServerMsg;
    use tokio::sync::oneshot;
    use tower::ServiceExt;

    use super::{ToolsFound, ToolsSinks, publish_then};
    use crate::AppState;
    use crate::downloader::tools::ToolPaths;

    fn sinks(state: &AppState) -> ToolsSinks {
        ToolsSinks {
            status: state.tools_status.clone(),
            paths: state.tool_paths.clone(),
            events: state.event_tx.clone(),
        }
    }

    fn ready_paths() -> ToolPaths {
        ToolPaths {
            ytdlp: PathBuf::from("tools").join("yt-dlp.exe"),
            ffmpeg: PathBuf::from("tools").join("ffmpeg.exe"),
            python: None,
            deno: Some(PathBuf::from("tools").join("deno.exe")),
        }
    }

    fn found() -> ToolsFound {
        ToolsFound {
            ytdlp_version: Some("2026.09.30".to_string()),
            js_runtime_ok: true,
            deno_version: Some("2.5.2".to_string()),
        }
    }

    /// The bug (#144): the startup task kept the `tools_status` WRITE guard
    /// through the yt-dlp self-update and the catalogue sweep, so after every
    /// start `GET /api/v1/status` (and a new dashboard socket) waited for
    /// both. Here the follow-ups are held open by the test, like a slow
    /// self-update; while they wait, neither lock may be held at all and the
    /// status route answers with the published tools.
    #[tokio::test]
    async fn the_status_answers_while_the_startup_follow_ups_run() {
        let state = crate::api::routes::tests::test_state().await;
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let follow_ups = async move {
            started_tx
                .send(())
                .expect("the test waits for the follow-ups");
            release_rx.await.is_ok()
        };
        let task = tokio::spawn(publish_then(
            sinks(&state),
            ready_paths(),
            found(),
            follow_ups,
        ));
        started_rx.await.expect("the follow-ups start");

        assert!(
            state.tools_status.try_write().is_ok(),
            "the tools status is still locked while the startup follow-ups run"
        );
        assert!(
            state.tool_paths.try_write().is_ok(),
            "the tool paths are still locked while the startup follow-ups run"
        );

        let request = Request::get("/api/v1/status").body(Body::empty()).unwrap();
        let answer = crate::api::router(state.clone(), None).oneshot(request);
        let response = tokio::time::timeout(Duration::from_secs(60), answer)
            .await
            .expect("GET /api/v1/status answers while the startup follow-ups run")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["tools"],
            serde_json::json!({
                "ytdlp_available": true,
                "ffmpeg_available": true,
                "ytdlp_version": "2026.09.30",
                "js_runtime_ok": true,
                "deno_version": "2.5.2",
            })
        );

        release_tx
            .send(())
            .expect("the follow-ups wait for the release");
        assert!(task.await.unwrap(), "the follow-ups ran to their end");
    }

    /// What the follow-ups — and so the download and lyrics workers they
    /// start — find when they begin: the event already sent to the open
    /// dashboards, the paths and every status field already set, and the
    /// status readable. Their own result is what `publish_then` returns.
    #[tokio::test]
    async fn the_ready_tools_are_published_before_the_follow_ups_run() {
        let state = crate::api::routes::tests::test_state().await;
        let mut events = state.event_tx.subscribe();
        let status = state.tools_status.clone();
        let paths = state.tool_paths.clone();
        let follow_ups = async move {
            let event = events.try_recv().ok();
            let ytdlp = paths
                .try_read()
                .ok()
                .and_then(|guard| guard.as_ref().map(|p| p.ytdlp.clone()));
            let status = status.try_read().ok().map(|s| {
                (
                    s.ytdlp_available,
                    s.ffmpeg_available,
                    s.ytdlp_version.clone(),
                    s.js_runtime_ok,
                    s.deno_version.clone(),
                )
            });
            (event, ytdlp, status)
        };
        let (event, ytdlp, status) =
            publish_then(sinks(&state), ready_paths(), found(), follow_ups).await;

        assert_eq!(
            event,
            Some(ServerMsg::ToolsStatus {
                ytdlp_available: true,
                ffmpeg_available: true,
                ytdlp_version: Some("2026.09.30".to_string()),
                js_runtime_ok: true,
                deno_version: Some("2.5.2".to_string()),
            })
        );
        assert_eq!(ytdlp, Some(ready_paths().ytdlp));
        assert_eq!(
            status,
            Some((
                true,
                true,
                Some("2026.09.30".to_string()),
                true,
                Some("2.5.2".to_string()),
            )),
            "the follow-ups found the tools status published and unlocked"
        );
    }
}
