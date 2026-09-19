//! HTTP client helpers for the REST API.
//!
//! All paths are relative (e.g. `/api/v1/playlists`); the browser resolves
//! them against the current origin automatically.

use gloo_net::http::Request;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sp_core::genlock::lock_state::LockState;

/// GET `path` and deserialise the JSON response.
pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let resp = Request::get(path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("GET {} → {}", path, resp.status()));
    }
    resp.json::<T>().await.map_err(|e| e.to_string())
}

/// POST JSON to `path` and deserialise the response.
pub async fn post_json<T: Serialize, R: DeserializeOwned>(
    path: &str,
    body: &T,
) -> Result<R, String> {
    let resp = Request::post(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("POST {} → {}", path, resp.status()));
    }
    resp.json::<R>().await.map_err(|e| e.to_string())
}

/// PUT JSON to `path` and deserialise the response.
pub async fn put_json<T: Serialize, R: DeserializeOwned>(
    path: &str,
    body: &T,
) -> Result<R, String> {
    let resp = Request::put(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("PUT {} → {}", path, resp.status()));
    }
    resp.json::<R>().await.map_err(|e| e.to_string())
}

/// PATCH JSON to `path` and deserialise the response.
#[allow(dead_code)]
pub async fn patch_json<T: Serialize, R: DeserializeOwned>(
    path: &str,
    body: &T,
) -> Result<R, String> {
    let resp = Request::patch(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("PATCH {} → {}", path, resp.status()));
    }
    resp.json::<R>().await.map_err(|e| e.to_string())
}

/// DELETE `path`.
pub async fn delete(path: &str) -> Result<(), String> {
    let resp = Request::delete(path)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("DELETE {} → {}", path, resp.status()));
    }
    Ok(())
}

/// POST `path` with no request body and discard the response body.
///
/// Used for playback control endpoints (`/api/v1/playback/{id}/{action}`)
/// that reply with `204 No Content`.
pub async fn post_empty(path: &str) -> Result<(), String> {
    let resp = Request::post(path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("POST {} → {}", path, resp.status()));
    }
    Ok(())
}

/// PUT JSON to `path` and discard the response body.
///
/// Used for playback mode updates and similar write endpoints that
/// reply with `204 No Content`.
pub async fn put_json_empty<T: Serialize>(path: &str, body: &T) -> Result<(), String> {
    let resp = Request::put(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("PUT {} → {}", path, resp.status()));
    }
    Ok(())
}

/// POST JSON to `path` and discard the response body.
///
/// Used for playback control endpoints that accept a JSON body but
/// reply with `204 No Content`.
pub async fn post_json_empty<T: Serialize>(path: &str, body: &T) -> Result<(), String> {
    let resp = Request::post(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("POST {} → {}", path, resp.status()));
    }
    Ok(())
}

// ── NDI genlock health (#150) ─────────────────────────────────────────────────

/// Serde default for [`NdiOutputHealth::lock_state`] — an absent field means
/// "not yet locked", the safe/honest fallback.
fn default_lock_state() -> LockState {
    LockState::Unlocked
}

/// dantesync clock health, the subset the badge tooltip shows. Every field
/// `#[serde(default)]` for forward compatibility; the server's `clock` object
/// carries more keys, which serde ignores.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub struct ClockView {
    #[serde(default)]
    pub is_locked: bool,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub offset_ns: Option<i64>,
    #[serde(default)]
    pub clock_ok: bool,
}

/// Boundary-pacing telemetry, the subset the badge tooltip shows.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub struct PacingView {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub late_frames: u64,
    #[serde(default)]
    pub jitter_p99_us: u64,
    #[serde(default)]
    pub repeats: u64,
    #[serde(default)]
    pub resyncs: u64,
    #[serde(default)]
    pub lag_slots: i64,
}

/// Audio clock-discipline telemetry, the subset the badge tooltip shows.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub struct AudioView {
    #[serde(default)]
    pub residual_ppm: f64,
    #[serde(default)]
    pub underruns: u64,
    /// #192 wall-clock audio emitter (SDK-clocked path).
    #[serde(default)]
    pub emitter: EmitterView,
}

/// Wall-clock audio-emitter telemetry (#192), the subset the badge tooltip
/// shows for the SDK-clocked path.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub struct EmitterView {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub silence_blocks: u64,
    #[serde(default)]
    pub ring_depth_ms: u64,
    #[serde(default)]
    pub emit_jitter_p99_us: u64,
    #[serde(default)]
    pub late_blocks: u64,
}

/// One NDI output's health as consumed by the dashboard's genlock badges
/// (#150). A read-only view over the server's `PipelineHealthSnapshot`; every
/// field `#[serde(default)]` so a partial or newer payload still deserialises
/// and unknown server fields are ignored.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct NdiOutputHealth {
    #[serde(default)]
    pub ndi_name: String,
    #[serde(default)]
    pub playlist_id: i64,
    /// Wire playback state (`Idle` / `WaitingForScene` / `Playing` / `Paused`).
    /// An output is LIVE on the wall iff this is `Playing`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub connections: i32,
    #[serde(default = "default_lock_state")]
    pub lock_state: LockState,
    #[serde(default)]
    pub lock_reason: String,
    #[serde(default)]
    pub clock: ClockView,
    #[serde(default)]
    pub pacing: PacingView,
    #[serde(default)]
    pub audio: AudioView,
    /// #196: server-set health reason (e.g. the dark-wall reason, "no OBS scene
    /// for this output", or "no receiver after restart"). The `HealthBar`
    /// counts the last for its NDI badge. A missing key deserializes to `None`.
    #[serde(default)]
    pub degraded_reason: Option<String>,
}

impl NdiOutputHealth {
    /// Whether this output is LIVE on the wall (`state == "Playing"`).
    pub fn is_live(&self) -> bool {
        self.state == "Playing"
    }
}

/// GET the per-output NDI genlock health snapshot.
pub async fn get_ndi_health() -> Result<Vec<NdiOutputHealth>, String> {
    get("/api/v1/ndi/health").await
}

/// #194 ROUND 3b: one Resolume push-chain host's health, as returned by
/// `GET /api/v1/resolume/health`. Moved here from `resolume_health.rs` (deleted
/// in favour of the shared `HealthBar`) so `store.resolume_health` can hold it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct HostHealth {
    pub host: String,
    #[serde(default)]
    pub last_refresh_ts: Option<String>,
    #[serde(default)]
    pub last_refresh_ok: bool,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub circuit_breaker_open: bool,
    #[serde(default)]
    pub clips_by_token: std::collections::BTreeMap<String, usize>,
}

impl HostHealth {
    /// Short human reason this host is unhealthy, or `None` if healthy. Same
    /// logic the old `ResolumeHealthCard` alert used — now folded into the
    /// `HealthBar` Resolume segment's tooltip.
    pub fn problem(&self) -> Option<String> {
        if self.circuit_breaker_open {
            return Some("okruh otvorený — Resolume nedostupné".into());
        }
        if self.consecutive_failures > 0 {
            return Some(format!(
                "obnova zlyháva ({} po sebe)",
                self.consecutive_failures
            ));
        }
        let missing: Vec<&str> = self
            .clips_by_token
            .iter()
            .filter(|(_, n)| **n == 0)
            .map(|(k, _)| k.as_str())
            .collect();
        if !missing.is_empty() {
            return Some(format!("chýbajúce klipy: {}", missing.join(", ")));
        }
        None
    }
}

/// GET the Resolume push-chain health snapshot (per configured host).
pub async fn get_resolume_health() -> Result<Vec<HostHealth>, String> {
    get("/api/v1/resolume/health").await
}

// ── Lyrics API helpers ────────────────────────────────────────────────────────

/// GET the lyrics pipeline queue status.
pub async fn get_lyrics_queue() -> Result<serde_json::Value, String> {
    get("/api/v1/lyrics/queue").await
}

/// GET the list of songs with their lyrics state.
///
/// Pass `playlist_id` to filter to a single playlist.
pub async fn get_lyrics_songs(playlist_id: Option<i64>) -> Result<Vec<serde_json::Value>, String> {
    let url = if let Some(pid) = playlist_id {
        format!("/api/v1/lyrics/songs?playlist_id={pid}")
    } else {
        "/api/v1/lyrics/songs".into()
    };
    get(&url).await
}

/// GET detailed lyrics info for a single video.
pub async fn get_lyrics_song_detail(video_id: i64) -> Result<serde_json::Value, String> {
    get(&format!("/api/v1/lyrics/songs/{video_id}")).await
}

/// POST to reprocess specific videos by ID.
pub async fn post_reprocess_videos(video_ids: &[i64]) -> Result<serde_json::Value, String> {
    post_json(
        "/api/v1/lyrics/reprocess",
        &serde_json::json!({ "video_ids": video_ids }),
    )
    .await
}

/// POST to reprocess all videos in a playlist.
pub async fn post_reprocess_playlist(playlist_id: i64) -> Result<serde_json::Value, String> {
    post_json(
        "/api/v1/lyrics/reprocess",
        &serde_json::json!({ "playlist_id": playlist_id }),
    )
    .await
}

/// POST to reprocess all stale lyrics entries.
pub async fn post_reprocess_all_stale() -> Result<serde_json::Value, String> {
    post_json("/api/v1/lyrics/reprocess-all-stale", &serde_json::json!({})).await
}

/// POST to clear the manual (bucket 0) lyrics queue.
pub async fn post_clear_manual_queue() -> Result<serde_json::Value, String> {
    post_json("/api/v1/lyrics/clear-manual-queue", &serde_json::json!({})).await
}

/// POST "Nesedí" feedback on a ★-flagged song (#142). Server clears
/// `lyrics_reference`, stamps the rejection timestamp, stores `note`, and
/// re-queues the song for reprocessing. Replies `204 No Content`.
pub async fn post_reference_feedback(video_id: i64, note: &str) -> Result<(), String> {
    post_json_empty(
        &format!("/api/v1/lyrics/songs/{video_id}/reference-feedback"),
        &serde_json::json!({ "note": note }),
    )
    .await
}

/// PATCH the per-song SK translation gender override (#152). `gender` is
/// `Some("m")`, `Some("f")`, or `None` (auto — clears the override back to the
/// masculine default). The server resets the song's translation version so the
/// worker re-translates it under the new gender. Replies `204 No Content`.
pub async fn patch_translation_gender(video_id: i64, gender: Option<&str>) -> Result<(), String> {
    patch_json_empty(
        &format!("/api/v1/lyrics/songs/{video_id}/translation-gender"),
        &serde_json::json!({ "gender": gender }),
    )
    .await
}

// ── Karaoke live control (#14) ────────────────────────────────────────────────

/// GET the live karaoke state: `{mode, vocal_gain, stems_pending, stems_done}`.
pub async fn get_karaoke() -> Result<serde_json::Value, String> {
    get("/api/v1/karaoke").await
}

/// POST a new karaoke mode + vocal gain (`0.0..=1.0`). Replies 204 No Content.
pub async fn post_karaoke(mode: &str, vocal_gain: f32) -> Result<(), String> {
    post_json_empty(
        "/api/v1/karaoke",
        &serde_json::json!({ "mode": mode, "vocal_gain": vocal_gain }),
    )
    .await
}

/// #177: re-enqueue a song for stem separation ("Zaradiť do fronty"). Replies
/// `{status, queue_position}`; we only need success/failure here.
pub async fn post_enqueue_stems(video_id: i64) -> Result<(), String> {
    let path = format!("/api/v1/stems/{video_id}/enqueue");
    let resp = Request::post(&path).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("POST {} → {}", path, resp.status()));
    }
    Ok(())
}

// ── Live playlist API helpers ─────────────────────────────────────────────────

/// GET all set-list items for a custom playlist.
pub async fn get_live_items(playlist_id: i64) -> Result<Vec<serde_json::Value>, String> {
    get(&format!("/api/v1/playlists/{playlist_id}/items")).await
}

/// POST to append a video to a custom playlist's set list.
pub async fn post_live_add_item(
    playlist_id: i64,
    video_id: i64,
) -> Result<serde_json::Value, String> {
    post_json(
        &format!("/api/v1/playlists/{playlist_id}/items"),
        &serde_json::json!({ "video_id": video_id }),
    )
    .await
}

/// DELETE a video from a custom playlist.
pub async fn delete_live_item(playlist_id: i64, video_id: i64) -> Result<(), String> {
    delete(&format!("/api/v1/playlists/{playlist_id}/items/{video_id}")).await
}

/// POST one-step reorder of a set-list row. `direction` must be `"up"`
/// (move earlier) or `"down"` (move later).
pub async fn post_live_move_item(
    playlist_id: i64,
    video_id: i64,
    direction: &str,
) -> Result<(), String> {
    post_json_empty(
        &format!("/api/v1/playlists/{playlist_id}/items/{video_id}/move"),
        &serde_json::json!({ "direction": direction }),
    )
    .await
}

/// POST to jump-and-play a specific video on a custom playlist.
///
/// When `position_ms` is `Some(ms)`, the server seeks atomically to that
/// offset before starting frame submission — eliminates the race between
/// a plain play-video + delayed seek dance (issue #88). Existing callers
/// that pass `None` retain the previous behaviour (play from 0).
pub async fn post_live_play_video(
    playlist_id: i64,
    video_id: i64,
    position_ms: Option<u64>,
) -> Result<(), String> {
    let mut body = serde_json::json!({ "video_id": video_id });
    if let Some(ms) = position_ms {
        body["position_ms"] = serde_json::json!(ms);
    }
    post_json_empty(
        &format!("/api/v1/playlists/{playlist_id}/play-video"),
        &body,
    )
    .await
}

/// POST seek to a playlist: `POST /api/v1/playback/{id}/seek {"position_ms":...}`.
/// Server returns 204 on success. v0.22.0 addition for the /live scrubber +
/// tap-a-line UI.
pub async fn seek_playlist(playlist_id: i64, position_ms: u64) -> Result<(), String> {
    let body = serde_json::json!({ "position_ms": position_ms });
    post_json_empty(
        &format!("/api/v1/playback/{playlist_id}/seek"),
        &body,
    )
    .await
}

/// GET the lyrics track for a video. Returns the full `LyricsTrack`
/// JSON — used by the LyricsScroller on /live to render a tappable
/// line list. 404 signals "no lyrics yet", surfaced as an Err string
/// so the UI can show an empty state.
pub async fn get_video_lyrics(video_id: i64) -> Result<sp_core::lyrics::LyricsTrack, String> {
    get(&format!("/api/v1/videos/{video_id}/lyrics")).await
}

// ── Import (v0.22.0) ──────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct ImportedVideo {
    pub video_id: i64,
    pub youtube_id: String,
    pub title: String,
}

/// POST a bare YouTube URL to the import endpoint. Returns 201 + the new
/// video_id on success. yt-dlp does the metadata fetch server-side.
pub async fn import_video(
    youtube_url: String,
    playlist_id: i64,
) -> Result<ImportedVideo, String> {
    let body = serde_json::json!({
        "youtube_url": youtube_url,
        "playlist_id": playlist_id,
    });
    post_json("/api/v1/videos/import", &body).await
}

/// PATCH `/api/v1/videos/{id}` with the `suppress_resolume_en` flag. Server
/// replies 204 on success. Used by the /live setlist "EN off" checkbox so
/// operators can flip the flag for songs that bake English lyrics into the
/// video (so SongPlayer won't push them again to Resolume's `#sp-subs` /
/// `#sp-subs-next` clips).
pub async fn patch_video_suppress_en(video_id: i64, suppress: bool) -> Result<(), String> {
    let body = serde_json::json!({ "suppress_resolume_en": suppress });
    patch_json_empty(&format!("/api/v1/videos/{video_id}"), &body).await
}

/// PATCH `/api/v1/videos/{id}` with corrected `song` + `artist` (#136 T1).
/// The server sanitizes both, rejects a whitespace-only song with 400, and
/// clears an empty artist to NULL. Server replies 204 on success. Used by
/// the dashboard video-list inline metadata editor so operators can fix the
/// wall title/subtitle for rows the metadata pipeline wrote wrong.
pub async fn patch_video_metadata(video_id: i64, song: &str, artist: &str) -> Result<(), String> {
    let body = serde_json::json!({ "song": song, "artist": artist });
    patch_json_empty(&format!("/api/v1/videos/{video_id}"), &body).await
}

// ── Dabing (#180) ───────────────────────────────────────────────────────────

/// GET the Dabing section: the seeded playlist id + every dub-requested video,
/// newest first.
pub async fn get_dabing() -> Result<serde_json::Value, String> {
    get("/api/v1/dabing").await
}

/// POST a bare YouTube URL to the Dabing import endpoint — downloads into the
/// Dabing playlist and flags it dub-requested. Returns 201 + the imported video.
pub async fn import_dabing(url: String) -> Result<ImportedVideo, String> {
    post_json("/api/v1/dabing/import", &serde_json::json!({ "url": url })).await
}

/// PATCH the per-video dub request flag (the row toggle). 204/404 → unit.
pub async fn patch_dub(video_id: i64, requested: bool) -> Result<(), String> {
    patch_json_empty(
        &format!("/api/v1/videos/{video_id}/dub"),
        &serde_json::json!({ "requested": requested }),
    )
    .await
}

/// PATCH the per-video mixer blend ratio (0.0..=1.0). Server replies 200 + the
/// stored (clamped) value; we only need success/failure here.
pub async fn patch_dub_mix(video_id: i64, ratio: f64) -> Result<(), String> {
    let resp = Request::patch(&format!("/api/v1/videos/{video_id}/dub-mix"))
        .json(&serde_json::json!({ "ratio": ratio }))
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("PATCH dub-mix → {}", resp.status()));
    }
    Ok(())
}

/// PATCH JSON to `path` and discard the response body. Mirror of
/// `put_json_empty` / `post_json_empty` for handlers that reply `204 No
/// Content`.
async fn patch_json_empty<T: Serialize>(path: &str, body: &T) -> Result<(), String> {
    let resp = Request::patch(path)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("PATCH {} → {}", path, resp.status()));
    }
    Ok(())
}
