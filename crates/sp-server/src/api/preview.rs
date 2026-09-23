//! Live video preview HTTP handler (#15 part 2). Extracted from `routes.rs`
//! to keep it under the 1000-line cap. The bytes are sampled opportunistically
//! from already-decoded frames by `playback::preview::PreviewTap` and never
//! touch the NDI submit / genlock / pacing path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tracing::{debug, info};

use crate::AppState;
use crate::playback::preview::fmp4_relay::FragmentRelay;
use crate::playback::preview::preview_encoder;
use crate::playback::preview::preview_stream::{StreamTap, ViewerGuard};

/// GET /api/v1/playback/{playlist_id}/preview.jpg — the live low-res video
/// preview of the currently-playing song for one playlist.
///
/// Returns `200 image/jpeg` with the latest sampled frame, `204 No Content`
/// when the pipeline is idle / has not produced a frame yet, or `404` when no
/// pipeline exists for that playlist. Each request marks a viewer (a TTL), so
/// the pipeline only spends any effort sampling while the dashboard is polling.
pub async fn get_playback_preview(
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    match state.preview_registry.get(playlist_id) {
        Some(tap) => {
            tap.note_viewer_request();
            match tap.latest_jpeg() {
                Some(jpeg) => (
                    StatusCode::OK,
                    [
                        (axum::http::header::CONTENT_TYPE, "image/jpeg"),
                        (axum::http::header::CACHE_CONTROL, "no-store"),
                    ],
                    jpeg,
                )
                    .into_response(),
                None => StatusCode::NO_CONTENT.into_response(),
            }
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// GET /api/v1/playback/{playlist_id}/preview.ws — the #178 live A/V preview
/// STREAM. Upgrades to a WebSocket that first sends the fragmented-MP4 init
/// segment (`ftyp`+`moov`) then each keyframe-aligned media fragment as a binary
/// message, for an MSE `<video>` in the browser. Subscribing marks a viewer
/// (spawning the encoder child on the first one, killing it after the last one
/// leaves); `404` for an unknown playlist, `503` when ffmpeg is not ready yet.
pub async fn get_playback_preview_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(playlist_id): Path<i64>,
) -> impl IntoResponse {
    let tap = match state.preview_registry.stream(playlist_id) {
        Some(t) => t,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let ffmpeg = state
        .tool_paths
        .read()
        .await
        .as_ref()
        .map(|t| t.ffmpeg.clone());
    let ffmpeg = match ffmpeg {
        Some(p) => p,
        None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    ws.on_upgrade(move |socket| handle_preview_ws(socket, tap, ffmpeg))
        .into_response()
}

/// One preview-stream WebSocket connection: register a viewer, ensure the
/// encoder child is running, send the init segment, then relay fragments until
/// the client disconnects.
async fn handle_preview_ws(mut socket: WebSocket, tap: StreamTap, ffmpeg: std::path::PathBuf) {
    // Registering the viewer (RAII) keeps the encoder child alive for the
    // duration of this connection; dropping `_guard` on return releases it.
    let (_guard, relay) = ViewerGuard::subscribe(&tap);
    preview_encoder::ensure_running(tap.shared().clone(), ffmpeg);
    let mut frag_rx = relay.subscribe();

    let init = match wait_for_init(&relay).await {
        Some(i) => i,
        None => {
            info!("preview.ws: encoder produced no init segment, closing");
            return;
        }
    };
    if socket
        .send(Message::Binary(init.to_vec().into()))
        .await
        .is_err()
    {
        return;
    }
    debug!(bytes = init.len(), "preview.ws: sent init segment");

    // #178 item 16: server-side keepalive + idle deadline. Ping every 5 s and
    // track the wall-time of the last message FROM the client; a half-open
    // client that never answers is dropped after 15 s of silence so it cannot
    // keep the encoder child alive forever. Browsers auto-answer Ping with Pong,
    // so the JS side needs no change.
    let start = Instant::now();
    let mut last_seen_ms: u64 = 0;
    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.tick().await; // consume the immediate first tick

    // #184 round F: a 1 Hz lag beacon. Each tick sends the media time the child
    // has produced since its Init (`FragmentRelay::produced_ms`) as a text frame;
    // the browser shim subtracts its buffered end to show how far the PICTURE is
    // behind the wall (vs a control that already applied on the wall in ~100 ms).
    let mut beacon = tokio::time::interval(Duration::from_secs(BEACON_INTERVAL_SECS));
    beacon.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            frag = frag_rx.recv() => match frag {
                Ok(bytes) => {
                    if socket.send(Message::Binary(bytes.to_vec().into())).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Viewer fell behind: skip the dropped fragments and resync
                    // on the next keyframe-aligned fragment (never blocks).
                    debug!(dropped = n, "preview.ws: viewer lagged, resyncing");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Ping(d))) => {
                    last_seen_ms = start.elapsed().as_millis() as u64;
                    let _ = socket.send(Message::Pong(d)).await;
                }
                Some(Ok(Message::Text(t))) => {
                    last_seen_ms = start.elapsed().as_millis() as u64;
                    // #184 round G: echo the shim's `{"ping":N}` at once on THIS
                    // socket, so the pong queues behind exactly the backlog the
                    // media is stuck in — the browser's round trip then measures
                    // the real transport lag (a tunnel backlog the beacon can't
                    // see). Any other text is ignored, as before.
                    if let Some(pong) = pong_frame(&t)
                        && socket.send(Message::Text(pong.into())).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(_)) => { last_seen_ms = start.elapsed().as_millis() as u64; }
                Some(Err(_)) => break,
            },
            _ = ping.tick() => {
                let now_ms = start.elapsed().as_millis() as u64;
                if is_idle(last_seen_ms, now_ms) {
                    info!("preview.ws: client idle > 15s with no frames, closing");
                    break;
                }
                if socket.send(Message::Ping(Vec::<u8>::new().into())).await.is_err() {
                    break;
                }
            }
            _ = beacon.tick() => {
                // #184 round F: 1 Hz lag beacon (see above).
                let produced = relay.produced_ms();
                if socket
                    .send(Message::Text(beacon_frame(produced).into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }
}

/// Server-side ping interval for the preview WS (#178 item 16).
const PING_INTERVAL_SECS: u64 = 5;
/// A client that sends NO message for longer than this is dropped (half-open).
const IDLE_TIMEOUT_MS: u64 = 15_000;
/// Interval of the #184 round-F preview lag beacon (1 Hz).
const BEACON_INTERVAL_SECS: u64 = 1;

/// The #184 round-F preview lag beacon frame: a JSON text message carrying how
/// many ms of media the encoder has produced since the child's Init
/// ([`FragmentRelay::produced_ms`]). The browser shim parses it and computes
/// `produced_ms/1000 − buffered_end` = how far the picture is behind the wall.
/// Pure so the exact JSON shape is unit-tested without a live socket / runtime.
fn beacon_frame(produced_ms: u64) -> String {
    format!("{{\"produced_ms\":{produced_ms}}}")
}

/// The #184 round-G answer to the browser shim's application-level ping: a
/// text frame `{"ping":N}` (N = the shim's own `performance.now()` ms, integer
/// or float) is answered with `{"pong":N}` carrying the SAME number verbatim,
/// so the shim's `rtt = now − pong` is clock-skew free. The number is echoed as
/// its RAW JSON text, byte-for-byte (serde_json's default float parsing is
/// best-effort, so a parse → re-serialize could shift a long-tail
/// `performance.now()` value by an ULP). Extra fields are ignored; anything that
/// is not a JSON object with a numeric `ping` → `None` (not answered). Browsers
/// expose no WS control-frame ping to JS, hence the application-level echo.
/// Pure so the exact shape is unit-tested.
fn pong_frame(text: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Ping<'a> {
        #[serde(borrow)]
        ping: &'a serde_json::value::RawValue,
    }
    let raw = serde_json::from_str::<Ping>(text).ok()?.ping.get();
    // Only a JSON number is a ping (not a string / null / bool / array).
    serde_json::from_str::<serde_json::Number>(raw).ok()?;
    Some(format!("{{\"pong\":{raw}}}"))
}

/// Whether the client has been silent past the idle deadline (#178 item 16):
/// strictly greater than [`IDLE_TIMEOUT_MS`] since its last message. Pure so the
/// deadline is unit-tested without a live socket/runtime.
fn is_idle(last_seen_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_seen_ms) > IDLE_TIMEOUT_MS
}

/// Poll for the child's init segment (`ftyp`+`moov`) for up to ~10 s while the
/// encoder starts and the first keyframe is produced.
async fn wait_for_init(relay: &FragmentRelay) -> Option<Arc<[u8]>> {
    for _ in 0..100 {
        if let Some(init) = relay.init() {
            return Some(init);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    relay.init()
}

#[cfg(test)]
#[path = "routes_tests_preview.rs"]
mod tests;
