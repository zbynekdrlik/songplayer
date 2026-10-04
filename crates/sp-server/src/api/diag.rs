//! `/api/v1/diag/*` (#223 S0): measurement routes for the box, not for the
//! dashboard. In its own file because `routes.rs` is at the 1000-line cap.
//!
//! `POST /api/v1/diag/decode-bench {"file": "<name>", "seconds": 1..=15}`
//! decodes `<data dir>/bench/<name>` through SongPlayer's real video decoder
//! and answers what one picture costs (`diag::decode_bench`):
//!
//! - 200: the report;
//! - 500: the report of a run the decoder ended, with its error and the
//!   pictures decoded so far;
//! - 400: a `file` that is not a bare name, or `seconds` outside 1..=15;
//! - 404: no such file in the sample dir;
//! - 409: another run is in progress;
//! - 501: a build without Media Foundation (Linux).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::AppState;
use crate::diag::decode_bench::{self, BenchFileError, BenchOutcome};

/// The body of `POST /api/v1/diag/decode-bench`.
#[derive(Debug, Deserialize)]
pub struct DecodeBenchRequest {
    /// A bare file name in `<data dir>/bench/`.
    pub file: String,
    /// The run's wall-time bound, 1 to 15 s.
    pub seconds: u64,
}

/// `POST /api/v1/diag/decode-bench`: check the request, take the bench, run
/// it on its own decode thread, and answer its report.
pub async fn post_decode_bench(
    State(state): State<AppState>,
    Json(req): Json<DecodeBenchRequest>,
) -> Response {
    let max_wall = match decode_bench::bench_seconds(req.seconds) {
        Ok(max_wall) => max_wall,
        Err(why) => return (StatusCode::BAD_REQUEST, why).into_response(),
    };
    let path = match state.decode_bench.resolve(&req.file) {
        Ok(path) => path,
        Err(BenchFileError::BadName(why)) => return (StatusCode::BAD_REQUEST, why).into_response(),
        Err(BenchFileError::Missing(path)) => {
            let why = format!("no bench file {}", path.display());
            return (StatusCode::NOT_FOUND, why).into_response();
        }
    };
    let Some(slot) = state.decode_bench.try_start() else {
        return (StatusCode::CONFLICT, "a decode-bench run is in progress").into_response();
    };
    match decode_bench::run(path, req.file, max_wall, slot).await {
        BenchOutcome::Report(report) => (report.status(), Json(report)).into_response(),
        BenchOutcome::Unsupported => (
            StatusCode::NOT_IMPLEMENTED,
            "decode-bench needs Windows Media Foundation",
        )
            .into_response(),
        BenchOutcome::Failed(why) => (StatusCode::INTERNAL_SERVER_ERROR, why).into_response(),
    }
}

#[cfg(test)]
#[path = "diag_tests.rs"]
mod tests;
