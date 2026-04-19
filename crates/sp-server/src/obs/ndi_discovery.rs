//! NDI source discovery — queries OBS for NDI inputs and matches against the
//! DB's active playlists to build the scene-detection map.
//!
//! This is the glue that was missing in the initial migration: `lib.rs`
//! used to create the `NdiSourceMap` as `HashMap::new()` and never populate
//! it, so [`scene::check_scene_items`] always returned an empty active set
//! and scene-driven playback never fired (issue #11).

use std::collections::HashMap;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use sqlx::{Row, SqlitePool};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::obs::text::{get_input_list_request, get_input_settings_request};

/// Query OBS for its NDI inputs and return a map of
/// `OBS input name → playlist_id` for the playlists whose `ndi_output_name`
/// matches an input's `ndi_source_name` setting.
///
/// The map is keyed by the OBS input name (e.g. `"sp-fast_video"`) — that is
/// what [`scene::check_scene_items`] compares against. The playlist's
/// `ndi_output_name` (e.g. `"SP-fast"`) is only used as the join key against
/// the OBS input's `ndi_source_name` setting.
/// Rebuild the OBS-input-name → playlist-id map.
///
/// Returns `None` when the rebuild **cannot be trusted** — typically a
/// transient OBS query failure. Callers MUST preserve the previously-built
/// map in that case, otherwise a single WebSocket hiccup wipes scene
/// detection and every `CurrentProgramSceneChanged` becomes a no-op
/// (silent playback stall; this is what broke the 2026-04-19 event).
///
/// Returns `Some(HashMap)` with the fresh mapping when the rebuild ran
/// end-to-end. An empty map is still a valid `Some`: it means the DB
/// genuinely has no active playlists, or OBS genuinely has no NDI
/// source inputs — both legitimate steady states.
pub async fn rebuild_ndi_source_map(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    pool: &SqlitePool,
) -> Option<HashMap<String, i64>> {
    let mut map = HashMap::new();

    // 1) Load active playlists {ndi_output_name → playlist_id} from DB.
    //    A DB read failure is a real signal something is wrong — return
    //    None so callers keep the last good map.
    let by_ndi_name = match load_playlist_ndi_names(pool).await {
        Ok(m) => m,
        Err(e) => {
            warn!("rebuild_ndi_source_map: failed to load playlists: {e}");
            return None;
        }
    };

    if by_ndi_name.is_empty() {
        debug!("rebuild_ndi_source_map: no active playlists with ndi_output_name");
        return Some(map);
    }

    // 2) Query OBS for NDI source inputs. A `None` return means the
    //    WebSocket response never arrived / was malformed — NOT that
    //    OBS truly has zero inputs. Treat as failure so the stale map
    //    survives until the next successful rebuild.
    let input_names = match fetch_ndi_input_names(write, read).await {
        Some(names) => names,
        None => {
            warn!(
                "rebuild_ndi_source_map: GetInputList returned nothing; \
                 keeping previous map so scene detection stays alive"
            );
            return None;
        }
    };

    if input_names.is_empty() {
        debug!("rebuild_ndi_source_map: OBS has no NDI source inputs");
        return Some(map);
    }

    // 3) For each OBS NDI input, read its settings to find the NDI sender name
    //    and match against the DB playlists.
    for input_name in input_names {
        let sender_name = match fetch_input_ndi_sender_name(write, read, &input_name).await {
            Some(s) => s,
            None => {
                debug!(
                    "rebuild_ndi_source_map: input '{input_name}' has no ndi_source_name setting"
                );
                continue;
            }
        };

        // The NDI plugin stores the full network-visible name, e.g.
        // `"RESOLUME-SNV (SP-fast)"` — machine hostname + the stream name
        // in parentheses. Extract the stream portion so we can match
        // against the playlist's `ndi_output_name` which is just the
        // bare stream name SongPlayer gave to its NdiLib sender.
        let stream_name = extract_ndi_stream_name(&sender_name);

        if let Some(&playlist_id) = by_ndi_name.get(stream_name) {
            debug!(
                "rebuild_ndi_source_map: '{input_name}' → playlist {playlist_id} (NDI sender '{sender_name}', stream '{stream_name}')"
            );
            map.insert(input_name, playlist_id);
        } else {
            debug!(
                "rebuild_ndi_source_map: no playlist matches NDI sender '{sender_name}' (stream '{stream_name}')"
            );
        }
    }

    info!(count = map.len(), "rebuilt NDI source map from OBS + DB");
    Some(map)
}

/// Extract the stream portion from an NDI network name.
///
/// NDI network names follow the format `"MACHINE (stream)"` where the
/// machine hostname is outside the parentheses and the stream name
/// SongPlayer gave to its NdiLib sender is inside. This function returns
/// the slice between the **first** ` (` and the final `)`, so nested
/// parentheses inside the stream name are preserved.
///
/// If the input has no `" ("` delimiter or does not end with `)`, it is
/// returned verbatim.
///
/// # NDI network name format
///
/// The official NDI SDK formats source names as `"<machine> (<stream>)"`
/// where `<machine>` is the host that owns the sender and `<stream>` is
/// the human-readable name the sender passed to `NDIlib_send_create`.
/// OBS's NDI input plugin stores this exact string in its
/// `ndi_source_name` input setting. Matching against the bare `<stream>`
/// part is therefore required to link an OBS NDI input back to a
/// SongPlayer playlist whose `ndi_output_name` is just the stream label.
///
/// Examples:
/// - `"RESOLUME-SNV (SP-fast)"` → `"SP-fast"`
/// - `"machine (name with spaces)"` → `"name with spaces"`
/// - `"SP-fast"` → `"SP-fast"` (no parentheses — return as-is)
/// - `"weird (inner (nested))"` → `"inner (nested)"` (first `(` wins)
pub(crate) fn extract_ndi_stream_name(full: &str) -> &str {
    // Find the first `" ("` delimiter; require the string to end with
    // `)`. Fall back to the raw name for any shape we don't recognise.
    if full.ends_with(')') {
        if let Some(open) = full.find(" (") {
            let inner_start = open + 2;
            let inner_end = full.len() - 1;
            if inner_end > inner_start {
                return &full[inner_start..inner_end];
            }
        }
    }
    full
}

/// Load the `{ndi_output_name → playlist_id}` map for all active playlists.
async fn load_playlist_ndi_names(pool: &SqlitePool) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, ndi_output_name FROM playlists \
         WHERE is_active = 1 AND ndi_output_name != ''",
    )
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = row.get("id");
        let ndi: String = row.get("ndi_output_name");
        map.insert(ndi, id);
    }
    Ok(map)
}

/// Issue `GetInputList` filtered to NDI sources and return the list of input
/// names. Returns `None` if the request failed or the response was malformed.
async fn fetch_ndi_input_names(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
) -> Option<Vec<String>> {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = get_input_list_request(&req_id);
    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
        warn!("fetch_ndi_input_names: send GetInputList failed: {e}");
        return None;
    }

    let response = wait_for_response(read, &req_id).await?;
    let arr = response["d"]["responseData"]["inputs"].as_array()?;

    Some(
        arr.iter()
            .filter_map(|v| v["inputName"].as_str().map(|s| s.to_string()))
            .collect(),
    )
}

/// Issue `GetInputSettings` for a single input and extract the
/// `ndi_source_name` setting (the NDI sender name that the OBS input receives
/// from). Returns `None` if the setting is absent.
async fn fetch_input_ndi_sender_name(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    input_name: &str,
) -> Option<String> {
    let req_id = uuid::Uuid::new_v4().to_string();
    let req = get_input_settings_request(&req_id, input_name);
    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
        warn!("fetch_input_ndi_sender_name: send GetInputSettings failed for {input_name}: {e}");
        return None;
    }

    let response = wait_for_response(read, &req_id).await?;
    response["d"]["responseData"]["inputSettings"]["ndi_source_name"]
        .as_str()
        .map(|s| s.to_string())
}

/// Read incoming WebSocket messages until a `RequestResponse` (op 7) with the
/// given request ID is found, then return it. Skips events and mismatched
/// responses. Returns `None` on close or after 100 iterations.
async fn wait_for_response(
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    request_id: &str,
) -> Option<serde_json::Value> {
    for _ in 0..100 {
        match read.next().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    let op = json["op"].as_u64().unwrap_or(u64::MAX);
                    if op == 7 && json["d"]["requestId"].as_str() == Some(request_id) {
                        return Some(json);
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                warn!("wait_for_response: WebSocket read error: {e}");
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ndi_stream_name_strips_machine_prefix() {
        assert_eq!(extract_ndi_stream_name("RESOLUME-SNV (SP-fast)"), "SP-fast");
        assert_eq!(extract_ndi_stream_name("WIN-BOX (SP-warmup)"), "SP-warmup");
        assert_eq!(
            extract_ndi_stream_name("dev-machine-1 (stream with spaces)"),
            "stream with spaces"
        );
    }

    #[test]
    fn extract_ndi_stream_name_passes_through_bare_names() {
        // Already bare — return as-is.
        assert_eq!(extract_ndi_stream_name("SP-fast"), "SP-fast");
        assert_eq!(extract_ndi_stream_name("no-parens"), "no-parens");
    }

    #[test]
    fn extract_ndi_stream_name_handles_empty_and_weird_inputs() {
        assert_eq!(extract_ndi_stream_name(""), "");
        // No space before the open paren → treat as opaque.
        assert_eq!(extract_ndi_stream_name("(just-parens)"), "(just-parens)");
        // find(" (") picks the FIRST ` (` so nested parens inside the
        // stream name are preserved, e.g. an SDK-generated fixup.
        assert_eq!(
            extract_ndi_stream_name("weird (inner (nested))"),
            "inner (nested)"
        );
        // A name that doesn't end with `)` is passed through untouched.
        assert_eq!(
            extract_ndi_stream_name("machine (incomplete"),
            "machine (incomplete"
        );
        // Empty parenthesised portion is passed through.
        assert_eq!(extract_ndi_stream_name("machine ()"), "machine ()");
    }

    #[tokio::test]
    async fn load_playlist_ndi_names_returns_active_with_non_empty_output() {
        let pool = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&pool).await.unwrap();

        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (1, 'a', 'u1', 'SP-a', 1), \
                    (2, 'b', 'u2', 'SP-b', 1), \
                    (3, 'c', 'u3', '', 1), \
                    (4, 'd', 'u4', 'SP-d', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let map = load_playlist_ndi_names(&pool).await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("SP-a"), Some(&1));
        assert_eq!(map.get("SP-b"), Some(&2));
        // playlist 3 has empty output name — excluded.
        // playlist 4 is inactive — excluded.
        assert!(!map.contains_key("SP-d"));
    }
}
