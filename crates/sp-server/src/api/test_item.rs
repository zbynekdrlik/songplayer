//! `/api/v1/test-item` (#228): camera-box's measurement clip as SongPlayer's
//! local test item (`crate::test_item`); #272: a pinned LIST of clips (the
//! wall measurement clips too, `test_item_clips.rs`), camera-box's the
//! default everywhere it was the only one.
//!
//! - `GET /api/v1/test-item` → `{imported, item, clip, sha256, clips}`:
//!   `item` = `{playlist_id, video_id, youtube_id, ndi_output_name, scene,
//!   duration_ms}` of camera-box's clip once imported, else `null`; `clip`
//!   the file name camera-box delivers and `sha256` its pinned sha256;
//!   `clips` = every listed clip `{name, file, fps, sha256, imported}`.
//! - `POST /api/v1/test-item/import` with `{"file": "<bare name>"}` imports
//!   `<data dir>/bench/<file>` (the box's sample dir) as the listed clip
//!   whose sha256 it has: `200 {outcome: "imported" | "already", clip,
//!   …item}`, then the engine is told to make the playlist's pipeline;
//!   `400` a name that is not bare, `404` no such file, `422 {error,
//!   expected, expected_any, found}` a file that is no listed clip (nothing
//!   written), `503` ffmpeg not ready yet, `500` ffmpeg, the cache or the
//!   store failed.
//! - The same route with NO body (or `{}`) imports every listed clip found
//!   in the sample dir under its own file name: `200 {clips: [{name, file,
//!   outcome: imported | already | absent | wrong_file | failed, found?,
//!   error?}]}` — an absent clip is no error; `500` with the same body when
//!   one failed; `503` before ffmpeg is ready.
//! - `POST /api/v1/test-item/start` plays a clip from position 0 (the
//!   engine's `PlayVideo`, as the play-video route does): no body (or `{}`)
//!   = camera-box's, `{"clip": "<name>"}` = that listed clip; `204`, `404`
//!   before that clip's import, `400` an unknown name or a body that does
//!   not read (fixed texts). `POST /api/v1/test-item/stop` pauses the test
//!   playlist: `204`, `404` while no clip is imported. Neither touches the
//!   program: its scene `sp-test` is pressed through the facade like any
//!   playlist's.
//!
//! The bodies are read as bytes and parsed into typed structs (unknown
//! fields refused), never through `serde_json::Value`.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::diag::decode_bench::BenchFileError;
use crate::test_item::{
    self, CLIPS, Clip, ClipImport, ClipOutcome, Ffmpeg, ImportError, ImportOutcome, TEST_CLIP_FILE,
    TestItem,
};
use crate::{AppState, EngineCommand};

/// The `400` of a start body that does not read.
pub const START_BODY_REFUSED: &str = "the body must be empty or {\"clip\": \"<name>\"}";

/// The `400` of a start naming no listed clip.
pub const UNKNOWN_CLIP: &str = "no such clip: GET /api/v1/test-item lists them";

/// The `400` of an import body that does not read.
pub const IMPORT_BODY_REFUSED: &str = "the body must be empty or {\"file\": \"<bare name>\"}";

/// `GET /api/v1/test-item`.
#[derive(Debug, Serialize)]
pub struct TestItemAnswer {
    pub imported: bool,
    pub item: Option<TestItem>,
    pub clip: &'static str,
    pub sha256: String,
    /// #272: every listed clip.
    pub clips: Vec<ClipState>,
}

/// One listed clip in `GET /api/v1/test-item`.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ClipState {
    pub name: &'static str,
    pub file: &'static str,
    pub fps: u32,
    pub sha256: String,
    pub imported: bool,
}

/// The body of `POST /api/v1/test-item/import` (none = the list import).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// A bare file name in the sample dir.
    #[serde(default)]
    pub file: Option<String>,
}

/// The body of `POST /api/v1/test-item/start` (none = camera-box's clip).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    /// A listed clip's name.
    #[serde(default)]
    pub clip: Option<String>,
}

/// The `200` of `POST /api/v1/test-item/import {"file"}`.
#[derive(Debug, Serialize)]
pub struct ImportAnswer {
    pub outcome: ImportOutcome,
    pub clip: &'static str,
    #[serde(flatten)]
    pub item: TestItem,
}

/// The answer of the list import.
#[derive(Debug, Serialize)]
pub struct ImportListAnswer {
    pub clips: Vec<ClipImport>,
}

/// The `422` of a file that is no listed clip.
#[derive(Debug, Serialize)]
pub struct WrongFileBody {
    pub error: &'static str,
    /// camera-box's clip's sha256 (the field #228 served).
    pub expected: String,
    /// #272: every listed clip's sha256.
    pub expected_any: Vec<String>,
    pub found: String,
}

/// A body that is empty or only whitespace reads as `T::default()`; any
/// other must be `T`'s JSON.
pub fn body_or_default<T: DeserializeOwned + Default>(body: &[u8]) -> serde_json::Result<T> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body)
}

/// The clip a start names: none = camera-box's, else a listed one.
pub fn start_clip(req: &StartRequest) -> Option<&'static Clip> {
    match req.clip.as_deref() {
        None => Some(&CLIPS[0]),
        Some(name) => test_item::clip_named(name),
    }
}

/// The list import's status: `500` when a clip failed, else `200`.
pub fn list_status(clips: &[ClipImport]) -> StatusCode {
    if clips.iter().any(|c| c.outcome == ClipOutcome::Failed) {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    }
}

/// Whether the list import left a clip in the test playlist (its pipeline
/// is then made).
pub fn list_imported_any(clips: &[ClipImport]) -> bool {
    clips
        .iter()
        .any(|c| matches!(c.outcome, ClipOutcome::Imported | ClipOutcome::Already))
}

/// The item a lookup found, or the `404` / `500` a route answers without it
/// (boxed: a `Response` is too large for an `Err`, clippy's
/// `result_large_err`).
fn found_or_refuse(
    found: Result<Option<TestItem>, sqlx::Error>,
    missing: String,
) -> Result<TestItem, Box<Response>> {
    match found {
        Ok(Some(item)) => Ok(item),
        Ok(None) => Err(Box::new((StatusCode::NOT_FOUND, missing).into_response())),
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

/// The box's ffmpeg, once the tools are ready.
async fn ready_ffmpeg(state: &AppState) -> Option<Ffmpeg> {
    let tools = state.tool_paths.read().await;
    tools.as_ref().map(|t| Ffmpeg(t.ffmpeg.clone()))
}

/// Every listed clip and whether it is imported.
async fn clip_states(state: &AppState) -> Result<Vec<ClipState>, sqlx::Error> {
    let mut states = Vec::with_capacity(CLIPS.len());
    for clip in &CLIPS {
        let imported = test_item::find_clip(&state.pool, clip).await?.is_some();
        states.push(ClipState {
            name: clip.name,
            file: clip.file,
            fps: clip.fps,
            sha256: clip.sha256_hex(),
            imported,
        });
    }
    Ok(states)
}

/// `GET /api/v1/test-item`.
pub async fn get_test_item(State(state): State<AppState>) -> Response {
    let read = async {
        let item = test_item::find(&state.pool).await?;
        Ok::<_, sqlx::Error>((item, clip_states(&state).await?))
    };
    match read.await {
        Ok((item, clips)) => Json(TestItemAnswer {
            imported: item.is_some(),
            item,
            clip: TEST_CLIP_FILE,
            sha256: test_item::clip_sha256(),
            clips,
        })
        .into_response(),
        Err(e) => {
            warn!(%e, "test item: reading it failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `POST /api/v1/test-item/import`.
pub async fn post_import(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(req) = body_or_default::<ImportRequest>(&body) else {
        warn!("test item: an import body that does not read — refused");
        return (StatusCode::BAD_REQUEST, IMPORT_BODY_REFUSED).into_response();
    };
    match req.file {
        Some(file) => import_file(&state, &file).await,
        None => import_list(&state).await,
    }
}

/// The import of one named file (#228's request).
async fn import_file(state: &AppState, file: &str) -> Response {
    let refuse = |status: StatusCode, why: String| {
        warn!(?file, %status, %why, "test item: not imported");
        (status, why).into_response()
    };
    let path = match state.decode_bench.resolve(file) {
        Ok(path) => path,
        Err(BenchFileError::BadName(why)) => {
            return refuse(StatusCode::BAD_REQUEST, why.to_string());
        }
        Err(BenchFileError::Missing(path)) => {
            let why = format!("no file {}", path.display());
            return refuse(StatusCode::NOT_FOUND, why);
        }
    };
    let Some(ffmpeg) = ready_ffmpeg(state).await else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "ffmpeg is not ready yet".to_string(),
        );
    };
    let imported = test_item::import(&state.pool, &state.cache_dir, &path, &CLIPS, &ffmpeg).await;
    match imported {
        Ok((outcome, clip, item)) => {
            info!(
                ?outcome,
                clip = clip.name,
                file = %path.display(),
                playlist_id = item.playlist_id,
                video_id = item.video_id,
                "test item: the measurement clip is the test item"
            );
            let ensure = EngineCommand::EnsurePipeline {
                playlist_id: item.playlist_id,
            };
            tell_engine(state, ensure).await;
            let clip = clip.name;
            Json(ImportAnswer {
                outcome,
                clip,
                item,
            })
            .into_response()
        }
        Err(ImportError::WrongFile(found)) => {
            warn!(file = %path.display(), %found, "test item: not a measurement clip — nothing written");
            let body = WrongFileBody {
                error: "not the measurement clip",
                expected: test_item::clip_sha256(),
                expected_any: CLIPS.iter().map(Clip::sha256_hex).collect(),
                found,
            };
            (StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response()
        }
        Err(e) => refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// The list import: every listed clip found in the sample dir.
async fn import_list(state: &AppState) -> Response {
    let Some(ffmpeg) = ready_ffmpeg(state).await else {
        warn!("test item: the list import waits for ffmpeg");
        return (StatusCode::SERVICE_UNAVAILABLE, "ffmpeg is not ready yet").into_response();
    };
    let bench = state.decode_bench.dir();
    let clips =
        test_item::import_listed(&state.pool, &state.cache_dir, bench, &CLIPS, &ffmpeg).await;
    for c in &clips {
        let (outcome, found, error) = (c.outcome, &c.found, &c.error);
        info!(
            clip = c.name,
            file = c.file,
            ?outcome,
            ?found,
            ?error,
            "test item: a listed clip"
        );
    }
    ensure_after_list(state, &clips).await;
    (list_status(&clips), Json(ImportListAnswer { clips })).into_response()
}

/// After the list import: the test playlist's pipeline is made when a clip
/// is in it.
async fn ensure_after_list(state: &AppState, clips: &[ClipImport]) {
    if !list_imported_any(clips) {
        return;
    }
    match test_item::find_any(&state.pool).await {
        Ok(Some(item)) => {
            let ensure = EngineCommand::EnsurePipeline {
                playlist_id: item.playlist_id,
            };
            tell_engine(state, ensure).await;
        }
        Ok(None) => warn!("test item: imported clips, yet no row reads back"),
        Err(e) => warn!(%e, "test item: reading the imported clips failed"),
    }
}

/// `POST /api/v1/test-item/start`: a clip from 0.
pub async fn post_start(State(state): State<AppState>, body: Bytes) -> Response {
    let Ok(req) = body_or_default::<StartRequest>(&body) else {
        warn!("test item: a start body that does not read — refused");
        return (StatusCode::BAD_REQUEST, START_BODY_REFUSED).into_response();
    };
    let Some(clip) = start_clip(&req) else {
        warn!("test item: a start naming no listed clip — refused");
        return (StatusCode::BAD_REQUEST, UNKNOWN_CLIP).into_response();
    };
    let found = test_item::find_clip(&state.pool, clip).await;
    let missing = format!("the clip {} is not imported", clip.name);
    let item = match found_or_refuse(found, missing) {
        Ok(item) => item,
        Err(answer) => return *answer,
    };
    info!(
        clip = clip.name,
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
    let found = test_item::find_any(&state.pool).await;
    let missing = "the test item is not imported".to_string();
    let item = match found_or_refuse(found, missing) {
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
