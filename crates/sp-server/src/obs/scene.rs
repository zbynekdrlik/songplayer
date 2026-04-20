//! Scene change handler — detects which NDI sources are active in a scene.

use std::collections::{HashMap, HashSet};

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};

use crate::obs::text::get_scene_items_request;

/// Check which NDI sources are present in a given scene.
///
/// Sends `GetSceneItemList` for the scene, checks each item name against the
/// NDI source map. Recurses into nested scenes / group sources.
pub async fn check_scene_items(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    scene_name: &str,
    ndi_sources: &HashMap<String, i64>,
) -> HashSet<i64> {
    let mut active_ids = HashSet::new();
    check_scene_items_recursive(write, read, scene_name, ndi_sources, &mut active_ids, 0).await;
    active_ids
}

/// Maximum recursion depth for nested scenes to prevent infinite loops.
const MAX_RECURSION_DEPTH: u32 = 5;

async fn check_scene_items_recursive(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    scene_name: &str,
    ndi_sources: &HashMap<String, i64>,
    active_ids: &mut HashSet<i64>,
    depth: u32,
) {
    if depth >= MAX_RECURSION_DEPTH {
        warn!("max scene recursion depth reached for '{scene_name}'");
        return;
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let req = get_scene_items_request(&request_id, scene_name);

    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
        warn!("failed to send GetSceneItemList: {e}");
        return;
    }

    // Read responses until we get our GetSceneItemList response.
    let items = match wait_for_response(read, &request_id).await {
        Some(response) => response,
        None => {
            warn!("no response for GetSceneItemList request");
            return;
        }
    };

    let scene_items = match items["d"]["responseData"]["sceneItems"].as_array() {
        Some(arr) => arr,
        None => return,
    };

    for item in scene_items {
        let source_name = match item["sourceName"].as_str() {
            Some(name) => name,
            None => continue,
        };

        // Check if this source matches an NDI source.
        if let Some(&playlist_id) = ndi_sources.get(source_name) {
            debug!("found NDI source '{source_name}' (playlist {playlist_id}) in '{scene_name}'");
            active_ids.insert(playlist_id);
        }

        // Check if this is a group or nested scene source and recurse.
        let is_group = item["isGroup"].as_bool().unwrap_or(false);
        let input_kind = item["inputKind"].as_str().unwrap_or("");
        let is_scene_source = input_kind == "scene" || is_group;

        if is_scene_source {
            debug!("recursing into nested scene/group '{source_name}'");
            Box::pin(check_scene_items_recursive(
                write,
                read,
                source_name,
                ndi_sources,
                active_ids,
                depth + 1,
            ))
            .await;
        }
    }
}

/// Wait for a `RequestResponse` (op 7) matching the given request ID.
///
/// Skips any event messages received while waiting. Returns `None` if the
/// connection closes before the response arrives.
async fn wait_for_response(
    read: &mut SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    request_id: &str,
) -> Option<serde_json::Value> {
    use std::time::Duration;
    // Bounded wait. Without the timeout, a dropped OBS response would block
    // this task on `read.next().await` and silently consume any next
    // message that arrived — including scene change events (see the
    // `rebuild_failure_does_not_wipe_ndi_source_map` regression test).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    for _ in 0..100 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            warn!("timed out waiting for response {request_id}");
            return None;
        }
        let next = match tokio::time::timeout(remaining, read.next()).await {
            Ok(msg) => msg,
            Err(_) => {
                warn!("timed out waiting for response {request_id}");
                return None;
            }
        };
        match next {
            Some(Ok(Message::Text(text))) => {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    let op = json["op"].as_u64().unwrap_or(u64::MAX);
                    if op == 7 && json["d"]["requestId"].as_str() == Some(request_id) {
                        return Some(json);
                    }
                    // Skip events and other responses while waiting.
                }
            }
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                warn!("WebSocket read error while waiting for response: {e}");
                return None;
            }
        }
    }
    warn!("timed out waiting for response {request_id}");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_recursion_depth_constant() {
        assert_eq!(MAX_RECURSION_DEPTH, 5);
    }

    #[test]
    fn test_parse_scene_items_response() {
        let response = serde_json::json!({
            "op": 7,
            "d": {
                "requestType": "GetSceneItemList",
                "requestId": "test-123",
                "requestStatus": { "result": true, "code": 100 },
                "responseData": {
                    "sceneItems": [
                        {
                            "sourceName": "NDI Source 1",
                            "sceneItemId": 1,
                            "isGroup": false,
                            "inputKind": "ndi_source"
                        },
                        {
                            "sourceName": "Nested Scene",
                            "sceneItemId": 2,
                            "isGroup": true,
                            "inputKind": ""
                        }
                    ]
                }
            }
        });

        let items = response["d"]["responseData"]["sceneItems"]
            .as_array()
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["sourceName"].as_str(), Some("NDI Source 1"));
        assert!(!items[0]["isGroup"].as_bool().unwrap());
        assert!(items[1]["isGroup"].as_bool().unwrap());
    }

    #[test]
    fn test_ndi_source_matching() {
        let mut ndi_sources = HashMap::new();
        ndi_sources.insert("NDI Source 1".to_string(), 42);
        ndi_sources.insert("Camera Feed".to_string(), 7);

        // Match found.
        assert_eq!(ndi_sources.get("NDI Source 1"), Some(&42));
        // No match.
        assert_eq!(ndi_sources.get("Unknown Source"), None);
    }

    #[test]
    fn test_scene_source_detection() {
        // A group source.
        let group_item = serde_json::json!({
            "sourceName": "My Group",
            "isGroup": true,
            "inputKind": ""
        });
        assert!(group_item["isGroup"].as_bool().unwrap_or(false));

        // A nested scene source.
        let scene_item = serde_json::json!({
            "sourceName": "Nested Scene",
            "isGroup": false,
            "inputKind": "scene"
        });
        assert_eq!(scene_item["inputKind"].as_str(), Some("scene"));

        // A regular source.
        let regular_item = serde_json::json!({
            "sourceName": "Webcam",
            "isGroup": false,
            "inputKind": "dshow_input"
        });
        let is_group = regular_item["isGroup"].as_bool().unwrap_or(false);
        let input_kind = regular_item["inputKind"].as_str().unwrap_or("");
        let is_scene_source = input_kind == "scene" || is_group;
        assert!(!is_scene_source);
    }
}
