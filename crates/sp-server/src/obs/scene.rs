//! Scene change handler — detects which NDI sources are active in a scene.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher, DispatcherError};
use crate::obs::text::get_scene_items_request;
use crate::obs::{NdiSourceMap, ObsEvent, ObsState, SharedWrite};

/// Apply a program-scene change: query which NDI sources are on program for
/// `scene_name`, write `current_scene` + `active_playlist_ids` into shared
/// state, and emit `ObsEvent::SceneChanged` for the engine bridge.
///
/// Shared by BOTH the `CurrentProgramSceneChanged` reader path and the ~2 s
/// poll-reconcile path (#170) so a dropped OBS event feeds the exact same
/// downstream handling. A duplicate emit for the already-current scene is
/// harmless — `(Playing, SceneOn)` is a no-op in the playback state machine.
pub async fn apply_scene_change(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    ndi_sources: &NdiSourceMap,
    state: &Arc<RwLock<ObsState>>,
    event_tx: &broadcast::Sender<ObsEvent>,
    scene_name: String,
) {
    let sources = ndi_sources.read().await;
    let active_ids = check_scene_items(write, dispatcher, &scene_name, &sources).await;
    drop(sources);

    {
        let mut s = state.write().await;
        s.current_scene = Some(scene_name.clone());
        s.active_playlist_ids = active_ids.clone();
    }

    let _ = event_tx.send(ObsEvent::SceneChanged {
        scene_name,
        active_playlist_ids: active_ids,
    });
}

/// Check which NDI sources are present in a given scene.
///
/// Sends `GetSceneItemList` for the scene, checks each item name against the
/// NDI source map. Recurses into nested scenes / group sources.
pub async fn check_scene_items(
    write: &SharedWrite,
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
    write: &SharedWrite,
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

    let items = match dispatcher
        .send_and_await(
            write,
            request_id,
            Message::Text(req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await
    {
        Ok(v) => v,
        Err(DispatcherError::Closed) => {
            warn!("no response for GetSceneItemList request (dispatcher closed)");
            return;
        }
        Err(DispatcherError::Timeout) => {
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
