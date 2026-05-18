//! Scene change handler — detects which NDI sources are active in a scene.

use std::collections::{HashMap, HashSet};

use futures::SinkExt;
use futures::stream::SplitSink;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher};
use crate::obs::text::get_scene_items_request;

/// Check which NDI sources are present in a given scene.
///
/// Sends `GetSceneItemList` for the scene, checks each item name against the
/// NDI source map. Recurses into nested scenes / group sources.
pub async fn check_scene_items(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    dispatcher: &Dispatcher,
    scene_name: &str,
    ndi_sources: &HashMap<String, i64>,
) -> HashSet<i64> {
    let mut active_ids = HashSet::new();
    check_scene_items_recursive(
        write,
        dispatcher,
        scene_name,
        ndi_sources,
        &mut active_ids,
        0,
    )
    .await;
    active_ids
}

/// Maximum recursion depth for nested scenes to prevent infinite loops.
const MAX_RECURSION_DEPTH: u32 = 5;

async fn check_scene_items_recursive(
    write: &mut SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    dispatcher: &Dispatcher,
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

    let rx = dispatcher.register(request_id.clone()).await;
    if let Err(e) = write.send(Message::Text(req.to_string().into())).await {
        warn!("failed to send GetSceneItemList: {e}");
        return;
    }

    let items = match tokio::time::timeout(DEFAULT_RESPONSE_TIMEOUT, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => {
            warn!("no response for GetSceneItemList request (dispatcher closed)");
            return;
        }
        Err(_) => {
            warn!("timed out waiting for GetSceneItemList for '{scene_name}'");
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

        if let Some(&playlist_id) = ndi_sources.get(source_name) {
            debug!("found NDI source '{source_name}' (playlist {playlist_id}) in '{scene_name}'");
            active_ids.insert(playlist_id);
        }

        let is_group = item["isGroup"].as_bool().unwrap_or(false);
        let input_kind = item["inputKind"].as_str().unwrap_or("");
        let is_scene_source = input_kind == "scene" || is_group;

        if is_scene_source {
            debug!("recursing into nested scene/group '{source_name}'");
            Box::pin(check_scene_items_recursive(
                write,
                dispatcher,
                source_name,
                ndi_sources,
                active_ids,
                depth + 1,
            ))
            .await;
        }
    }
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
