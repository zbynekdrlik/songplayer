//! Shared bare-URL import core (#180). Extracted out of `api/routes.rs` (which
//! sits at the 1000-line cap) so BOTH the generic `POST /api/v1/videos/import`
//! handler and the Dabing section's `POST /api/v1/dabing/import` reuse the exact
//! same yt-dlp metadata fetch + `videos` upsert, without either handler growing
//! `routes.rs`. Extracting the body here NET-REDUCES `routes.rs`.

use axum::http::StatusCode;
use sqlx::Row;

use crate::AppState;
use crate::api::routes::ImportVideoResp;

/// Fetch a bare YouTube URL's metadata and upsert a `videos` row into
/// `playlist_id` (`normalized=0`, so the download worker picks it up within
/// ~5 s). Returns the new/updated row summary, or `(status, message)` on any
/// failure (bad URL, tools not ready, yt-dlp error, DB error).
///
/// The metadata fetch is threaded the production cookie jar
/// (`cookies.txt` in the data dir, i.e. `cache_dir`'s parent —
/// `C:\ProgramData\SongPlayer\cookies.txt`, the same file the download path
/// reads) when it exists (#180 addendum) so it clears the YouTube bot-check
/// exactly as `download_video_stream` does — otherwise the box import fails
/// "Sign in to confirm you're not a bot".
pub(crate) async fn import_video_core(
    state: &AppState,
    youtube_url: &str,
    playlist_id: i64,
) -> Result<ImportVideoResp, (StatusCode, String)> {
    use crate::downloader::tools::{extract_youtube_id, fetch_video_metadata};

    // Fast reject obviously non-YouTube URLs before shelling out.
    if extract_youtube_id(youtube_url).is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "URL does not look like a YouTube video link".to_string(),
        ));
    }

    let ytdlp_path = {
        let guard = state.tool_paths.read().await;
        match guard.as_ref() {
            Some(tp) => tp.ytdlp.clone(),
            None => {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "yt-dlp not ready yet on this server".to_string(),
                ));
            }
        }
    };

    // Attach the production cookie jar when present (#180 addendum). It lives in
    // the data dir alongside the DB — `cache_dir`'s parent, since the app always
    // sets `cache_dir = <data_dir>/cache` (src-tauri/lib.rs). Falls back to the
    // cache_dir itself if it has no parent (a bare relative path in tests).
    let cookies_path = state
        .cache_dir
        .parent()
        .unwrap_or(state.cache_dir.as_path())
        .join("cookies.txt");
    let cookies = cookies_path.exists().then_some(cookies_path.as_path());

    let meta = match fetch_video_metadata(&ytdlp_path, youtube_url, cookies).await {
        Ok(m) => m,
        Err(e) => return Err((StatusCode::BAD_REQUEST, format!("yt-dlp failed: {e}"))),
    };

    let row = sqlx::query(
        "INSERT INTO videos (playlist_id, youtube_id, title, duration_ms, normalized) \
         VALUES (?, ?, ?, ?, 0) \
         ON CONFLICT(playlist_id, youtube_id) DO UPDATE SET title = excluded.title \
         RETURNING id",
    )
    .bind(playlist_id)
    .bind(&meta.youtube_id)
    .bind(&meta.title)
    .bind(meta.duration_ms.map(|ms| ms as i64))
    .fetch_one(&state.pool)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(ImportVideoResp {
        video_id: row.get(0),
        youtube_id: meta.youtube_id,
        title: meta.title,
    })
}
