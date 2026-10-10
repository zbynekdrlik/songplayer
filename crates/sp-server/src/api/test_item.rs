//! `/api/v1/test-item` (#228): camera-box's measurement clip as SongPlayer's
//! local test item (`crate::test_item`).
//!
//! - `GET /api/v1/test-item` → `{imported, item, clip, sha256}`: `item` =
//!   `{playlist_id, video_id, youtube_id, ndi_output_name, scene,
//!   duration_ms}` once imported, else `null`; `clip` the file name camera-box
//!   delivers and `sha256` the pinned clip's.
//! - `POST /api/v1/test-item/import {"file": "<bare name>"}` imports
//!   `<data dir>/bench/<file>` (the box's sample dir): `200 {outcome:
//!   "imported" | "already", …item}`, then the engine is told to make the
//!   playlist's pipeline; `400` a name that is not bare, `404` no such file,
//!   `422 {error, expected, found}` a file that is not the pinned clip
//!   (nothing written), `503` ffmpeg not ready yet, `500` ffmpeg, the cache
//!   or the store failed.
//! - `POST /api/v1/test-item/start` plays the item from position 0 (the
//!   engine's `PlayVideo`, as the play-video route does), `POST
//!   /api/v1/test-item/stop` pauses its playlist: `204`, `404` before the
//!   import. Neither touches the program: its scene `sp-test` is pressed
//!   through the facade like any playlist's.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::diag::decode_bench::BenchFileError;
use crate::test_item::{self, Ffmpeg, ImportError, ImportOutcome, TEST_CLIP_FILE, TestItem};
use crate::{AppState, EngineCommand};

/// `GET /api/v1/test-item`.
#[derive(Debug, Serialize)]
pub struct TestItemAnswer {
    pub imported: bool,
    pub item: Option<TestItem>,
    pub clip: &'static str,
    pub sha256: String,
}

/// The body of `POST /api/v1/test-item/import`.
#[derive(Debug, Deserialize)]
pub struct ImportRequest {
    /// A bare file name in the sample dir.
    pub file: String,
}

/// The `200` of `POST /api/v1/test-item/import`.
#[derive(Debug, Serialize)]
pub struct ImportAnswer {
    pub outcome: ImportOutcome,
    #[serde(flatten)]
    pub item: TestItem,
}

/// The `422` of a file that is not the pinned clip.
#[derive(Debug, Serialize)]
pub struct WrongFileBody {
    pub error: &'static str,
    pub expected: String,
    pub found: String,
}

/// The test item, or the `404` / `500` a route answers without it (boxed:
/// a `Response` is too large for an `Err`, clippy's `result_large_err`).
async fn imported(state: &AppState) -> Result<TestItem, Box<Response>> {
    match test_item::find(&state.pool).await {
        Ok(Some(item)) => Ok(item),
        Ok(None) => Err(Box::new(
            (StatusCode::NOT_FOUND, "the test item is not imported").into_response(),
        )),
        Err(e) => {
            warn!(%e, "test item: reading it failed");
            Err(Box::new(
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            ))
        }
    }
}

/// Tell the engine; a closed channel (the engine is gone) is WARNed.
async fn tell_engine(state: &AppState, command: EngineCommand) {
    if let Err(e) = state.engine_tx.send(command).await {
        warn!(%e, "test item: the engine is not listening");
    }
}

/// `GET /api/v1/test-item`.
pub async fn get_test_item(State(state): State<AppState>) -> Response {
    match test_item::find(&state.pool).await {
        Ok(item) => Json(TestItemAnswer {
            imported: item.is_some(),
            item,
            clip: TEST_CLIP_FILE,
            sha256: test_item::clip_sha256(),
        })
        .into_response(),
        Err(e) => {
            warn!(%e, "test item: reading it failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `POST /api/v1/test-item/import`.
pub async fn post_import(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Response {
    let refuse = |status: StatusCode, why: String| {
        warn!(file = ?req.file, %status, %why, "test item: not imported");
        (status, why).into_response()
    };
    let clip = match state.decode_bench.resolve(&req.file) {
        Ok(clip) => clip,
        Err(BenchFileError::BadName(why)) => {
            return refuse(StatusCode::BAD_REQUEST, why.to_string());
        }
        Err(BenchFileError::Missing(path)) => {
            let why = format!("no file {}", path.display());
            return refuse(StatusCode::NOT_FOUND, why);
        }
    };
    let ffmpeg = state
        .tool_paths
        .read()
        .await
        .as_ref()
        .map(|t| t.ffmpeg.clone());
    let Some(ffmpeg) = ffmpeg else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "ffmpeg is not ready yet".to_string(),
        );
    };
    let expected = test_item::clip_sha256();
    let imported = test_item::import(
        &state.pool,
        &state.cache_dir,
        &clip,
        &expected,
        &Ffmpeg(ffmpeg),
    )
    .await;
    match imported {
        Ok((outcome, item)) => {
            info!(
                ?outcome,
                file = %clip.display(),
                playlist_id = item.playlist_id,
                video_id = item.video_id,
                "test item: the measurement clip is the test item"
            );
            let ensure = EngineCommand::EnsurePipeline {
                playlist_id: item.playlist_id,
            };
            tell_engine(&state, ensure).await;
            Json(ImportAnswer { outcome, item }).into_response()
        }
        Err(ImportError::WrongFile(found)) => {
            warn!(file = %clip.display(), %found, "test item: not the measurement clip — nothing written");
            let body = WrongFileBody {
                error: "not the measurement clip",
                expected,
                found,
            };
            (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
        }
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// `POST /api/v1/test-item/start`: the item from position 0.
pub async fn post_start(State(state): State<AppState>) -> Response {
    let item = match imported(&state).await {
        Ok(item) => item,
        Err(answer) => return *answer,
    };
    info!(
        playlist_id = item.playlist_id,
        video_id = item.video_id,
        "test item: start from 0"
    );
    let play = EngineCommand::PlayVideo {
        playlist_id: item.playlist_id,
        video_id: item.video_id,
        position_ms: Some(0),
    };
    tell_engine(&state, play).await;
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /api/v1/test-item/stop`: its playlist paused.
pub async fn post_stop(State(state): State<AppState>) -> Response {
    let item = match imported(&state).await {
        Ok(item) => item,
        Err(answer) => return *answer,
    };
    info!(playlist_id = item.playlist_id, "test item: stop");
    let pause = EngineCommand::Pause {
        playlist_id: item.playlist_id,
    };
    tell_engine(&state, pause).await;
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
#[path = "test_item_tests.rs"]
mod tests;
