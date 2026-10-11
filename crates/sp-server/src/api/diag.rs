//! `/api/v1/diag/*` (#223 S0): measurement routes for the box, not for the
//! dashboard. In its own file because `routes.rs` is at the 1000-line cap.
//!
//! `POST /api/v1/diag/decode-bench {"file": "<name>", "seconds": 1..=15}`
//! decodes `<data dir>/bench/<name>` through SongPlayer's real video decoder
//! and answers what one picture costs (`diag::decode_bench`). #223 S3b: an
//! optional `"hw": true` decodes on the GPU (the reader's `Hardware` mode);
//! the report says which path really decoded:
//!
//! - 200: the report;
//! - 500 (JSON): the report of a run the decoder ended, with its error and
//!   the pictures decoded so far;
//! - 500 (text): a run with no report, its thread did not start or panicked;
//! - 400: a `file` that is not a bare name, or `seconds` outside 1..=15;
//! - 404: no such file in the sample dir;
//! - 409: another run is in progress;
//! - 501: a build without Media Foundation (Linux).
//!
//! axum answers a body that is not JSON (400), not the two fields or a `hw`
//! that is not a bool (422), or not `application/json` (415) before this
//! handler runs.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracing::{info, warn};

use sp_decoder::DecodeMode;

use crate::AppState;
use crate::diag::decode_bench::{self, BenchFileError, BenchOutcome, BenchReport};

/// The body of `POST /api/v1/diag/decode-bench`.
#[derive(Debug, Deserialize)]
pub struct DecodeBenchRequest {
    /// A bare file name in `<data dir>/bench/`.
    pub file: String,
    /// The run's wall-time bound, 1 to 15 s.
    pub seconds: u64,
    /// #223 S3b: decode on the GPU (`Hardware` mode). Absent = software.
    #[serde(default)]
    pub hw: bool,
}

/// `POST /api/v1/diag/decode-bench`: check the request, take the bench, run
/// it on its own decode thread, and answer its report.
pub async fn post_decode_bench(
    State(state): State<AppState>,
    Json(req): Json<DecodeBenchRequest>,
) -> Response {
    // Every refusal says why, and logs it (a failed run is a WARN below).
    let refuse = |status: StatusCode, why: String| {
        info!(file = ?req.file, seconds = req.seconds, hw = req.hw, %status, %why, "decode-bench: no report");
        (status, why).into_response()
    };
    let max_wall = match decode_bench::bench_seconds(req.seconds) {
        Ok(max_wall) => max_wall,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, why.to_string()),
    };
    let path = match state.decode_bench.resolve(&req.file) {
        Ok(path) => path,
        Err(BenchFileError::BadName(why)) => {
            return refuse(StatusCode::BAD_REQUEST, why.to_string());
        }
        Err(BenchFileError::Missing(path)) => {
            let why = format!("no bench file {}", path.display());
            return refuse(StatusCode::NOT_FOUND, why);
        }
    };
    let Some(slot) = state.decode_bench.try_start() else {
        let why = "a decode-bench run is in progress".to_string();
        return refuse(StatusCode::CONFLICT, why);
    };
    let mode = DecodeMode::from_hw_flag(req.hw);
    match decode_bench::run(path, req.file.clone(), max_wall, mode, slot).await {
        BenchOutcome::Report(report) => (report_status(&report), Json(report)).into_response(),
        BenchOutcome::Unsupported => {
            let why = "decode-bench needs Windows Media Foundation".to_string();
            refuse(StatusCode::NOT_IMPLEMENTED, why)
        }
        BenchOutcome::Failed(why) => {
            warn!(file = ?req.file, %why, "decode-bench: the run failed");
            (StatusCode::INTERNAL_SERVER_ERROR, why).into_response()
        }
    }
}

/// The body of `POST /api/v1/diag/fit-bench` (#223 S10a).
#[derive(Debug, Deserialize)]
pub struct FitBenchRequest {
    /// The source picture's size: even, up to 3840×2160.
    pub width: u32,
    pub height: u32,
    /// Fits (and fades) to time, 1 to 600.
    pub frames: u32,
}

/// `POST /api/v1/diag/fit-bench` (#223 S10a, `diag::fit_bench`): the program
/// canvas fit and fade of a `width`×`height` picture on the box. 200 with the
/// report; 400 for a size or count out of range; 409 while a bench (this one
/// or the decode bench: one at a time) runs; 500 when its thread failed.
pub async fn post_fit_bench(
    State(state): State<AppState>,
    Json(req): Json<FitBenchRequest>,
) -> Response {
    use crate::diag::fit_bench;
    let refuse = |status: StatusCode, why: String| {
        info!(width = req.width, height = req.height, frames = req.frames, %status, %why, "fit-bench: no report");
        (status, why).into_response()
    };
    if let Err(why) = fit_bench::check(req.width, req.height, req.frames) {
        return refuse(StatusCode::BAD_REQUEST, why.to_string());
    }
    let Some(slot) = state.decode_bench.try_start() else {
        return refuse(
            StatusCode::CONFLICT,
            "a bench run is in progress".to_string(),
        );
    };
    let bands =
        crate::playback::program_transition::mix_bands(crate::lyrics::heavy_slot::logical_cores());
    let (width, height, frames) = (req.width, req.height, req.frames);
    info!(width, height, frames, bands, "fit-bench: start");
    let run = tokio::task::spawn_blocking(move || {
        let report = fit_bench::run(width, height, frames, bands);
        drop(slot);
        report
    })
    .await;
    match run {
        Ok(report) => {
            info!(
                width, height, frames, bands = report.bands,
                fit_us = ?report.fit_us, fade_us = ?report.fade_us,
                over_budget = report.over_budget, "fit-bench: done"
            );
            Json(report).into_response()
        }
        Err(e) => {
            warn!(%e, "fit-bench: the run failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("fit-bench failed: {e}"),
            )
                .into_response()
        }
    }
}

/// A report's status: 500 when the decoder failed (the body still carries
/// the report), else 200.
fn report_status(report: &BenchReport) -> StatusCode {
    if report.failed() {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    }
}

#[cfg(test)]
#[path = "diag_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "diag_tests_fit.rs"]
mod tests_fit;
