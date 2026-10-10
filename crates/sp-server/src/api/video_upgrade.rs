//! #223 S11: `POST /api/v1/video-upgrade {"youtube_id"}` — upgrade ONE
//! cached song's video in place now (`video_upgrade::run`, design comment
//! 6103060545). The pilot's route: S12's worker runs the same upgrade by
//! itself.
//!
//! Answers 200 with the report (`outcome` `upgraded` / `no_better` / `busy`
//! / `failed`, the old and new facts, what was resolved, `error`,
//! `elapsed_ms`); 400 for a value that is not a YouTube id; 501 off Windows
//! (the reader that plays the video is Media Foundation); 503 before the
//! tools are ready; 409 while another upgrade runs. The upgrade runs as its
//! own task, so a client that hangs up never stops it half-way.

use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::AppState;
use crate::downloader::{format, tools::is_yt_id};
use crate::video_upgrade::{self, UpgradeReport, steps::Real};

/// One upgrade at a time per process.
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
pub(crate) struct UpgradeRequest {
    youtube_id: String,
}

#[derive(Serialize)]
struct Answer {
    #[serde(flatten)]
    report: UpgradeReport,
    elapsed_ms: u64,
}

fn refusal(status: StatusCode, error: &str) -> Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

/// `POST /api/v1/video-upgrade` (module doc).
#[cfg_attr(test, mutants::skip)] // glue around `video_upgrade::run`; the refusals are tested through the router
pub(crate) async fn upgrade(
    State(state): State<AppState>,
    Json(req): Json<UpgradeRequest>,
) -> Response {
    let youtube_id = req.youtube_id.trim().to_string();
    if !is_yt_id(&youtube_id) {
        info!("video upgrade: refused, not a YouTube id");
        return refusal(StatusCode::BAD_REQUEST, "youtube_id must be a YouTube id");
    }
    if !cfg!(windows) {
        return refusal(
            StatusCode::NOT_IMPLEMENTED,
            "the video reader is Windows only",
        );
    }
    let tools = state.tool_paths.read().await.clone();
    let Some(tools) = tools else {
        return refusal(
            StatusCode::SERVICE_UNAVAILABLE,
            "yt-dlp is not ready yet on this server",
        );
    };
    let Ok(slot) = RUNNING.try_lock() else {
        info!(youtube_id = %youtube_id, "video upgrade: refused, another one runs");
        return refusal(StatusCode::CONFLICT, "another video upgrade runs");
    };
    let cap = format::live_cap(&state.pool).await;
    let cookies = state
        .cache_dir
        .parent()
        .unwrap_or(state.cache_dir.as_path())
        .join("cookies.txt");
    let steps = Real {
        ytdlp: tools.ytdlp.clone(),
        ffmpeg_dir: tools
            .ffmpeg
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf(),
        cookies: cookies.exists().then_some(cookies),
        mode: crate::playback::video_decode::global().mode(),
    };
    let pool = state.pool.clone();
    let cache_dir = state.cache_dir.clone();
    let task = tokio::spawn(async move {
        let _slot = slot;
        let started = Instant::now();
        info!(youtube_id = %youtube_id, cap, "video upgrade: start");
        let report = video_upgrade::run(
            &pool,
            &cache_dir,
            &youtube_id,
            cap,
            &steps,
            crate::peer::wire::now_ms(),
        )
        .await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let old = report.old.map(|f| (f.width, f.height));
        let new = report.new.map(|f| (f.width, f.height));
        match &report.error {
            None => info!(
                youtube_id = %youtube_id,
                outcome = ?report.outcome,
                ?old,
                ?new,
                format_id = report.resolved.as_ref().map(|f| f.format_id.as_str()),
                elapsed_ms,
                "video upgrade: done"
            ),
            Some(error) => warn!(
                youtube_id = %youtube_id,
                outcome = ?report.outcome,
                ?old,
                ?new,
                elapsed_ms,
                "video upgrade: not upgraded: {error}"
            ),
        }
        Answer { report, elapsed_ms }
    });
    match task.await {
        Ok(answer) => Json(answer).into_response(),
        Err(e) => {
            warn!("video upgrade: the task failed: {e}");
            refusal(StatusCode::INTERNAL_SERVER_ERROR, "the upgrade task failed")
        }
    }
}

#[cfg(test)]
#[path = "video_upgrade_tests.rs"]
mod tests;
