//! Scene change handler — detects which NDI sources are active in a scene.
//!
//! #218: a scene's playlist lookup that FAILED (no answer, the connection
//! closed, or an answer without a `sceneItems` list) is not a scene that
//! shows no playlist. [`check_scene_items`] returns it as a [`LookupError`];
//! [`apply_scene_change`] then keeps the previous playlists, records the
//! failure in `ObsState::lookup_failed` and broadcasts nothing, and the ~2 s
//! scene poll (`scene_poll.rs`) looks the scene up again until it answers.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::obs::dispatcher::{DEFAULT_RESPONSE_TIMEOUT, Dispatcher, DispatcherError};
use crate::obs::snapshot::ObsShared;
use crate::obs::text::get_scene_items_request;
use crate::obs::{NdiSourceMap, ObsEvent, SharedWrite};

/// Why a scene's playlist lookup failed: its playlists are UNKNOWN, never
/// "none" (#218).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupError {
    /// The OBS connection closed before cg OBS answered.
    Closed,
    /// cg OBS did not answer within the response timeout.
    Timeout,
    /// cg OBS refused the request (`requestStatus.result` false).
    Refused { code: u64, comment: String },
    /// cg OBS answered, but without a `sceneItems` list.
    NoSceneItems,
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "the OBS connection closed before the answer"),
            Self::Timeout => write!(f, "no answer in time"),
            Self::Refused { code, comment } => write!(f, "refused ({code}: {comment})"),
            Self::NoSceneItems => write!(f, "answered without a sceneItems list"),
        }
    }
}

impl From<DispatcherError> for LookupError {
    fn from(e: DispatcherError) -> Self {
        match e {
            DispatcherError::Closed => Self::Closed,
            DispatcherError::Timeout => Self::Timeout,
        }
    }
}

/// Apply a program-scene change: query which NDI sources are on program for
/// `scene_name`, write `current_scene` + `active_playlist_ids` into shared
/// state, and emit `ObsEvent::SceneChanged` (#221 L4b: no engine bridge reads
/// it any more; L6 deletes the scene detection).
///
/// Shared by BOTH the `CurrentProgramSceneChanged` reader path and the ~2 s
/// poll-reconcile path (#170) so a dropped OBS event feeds the exact same
/// downstream handling. A duplicate emit for the already-current scene is
/// harmless — `(Playing, SceneOn)` is a no-op in the playback state machine.
///
/// #218: when the lookup FAILS, the scene's name is stored but its playlists
/// are not: `active_playlist_ids` keeps the previous set, `lookup_failed`
/// names the scene, and nothing is broadcast (an empty set would scene-off
/// the playlist on program). The scene poll looks it up again; a success
/// clears `lookup_failed` and broadcasts as usual.
///
/// `ticket` ([`ObsShared::scene_ticket`], taken before `scene_name` was read:
/// the event's, or the poll's / the initial `GetCurrentProgramScene`): an
/// answer that arrives after a later apply's answer was written is dropped —
/// cg OBS may answer lookups out of order, the poll's relookups can overlap
/// an event's lookup, and an event can overtake a poll's read. The
/// `SceneChanged` goes out under the same write lock, so the engine gets the
/// scene changes in the order they were written.
pub(crate) async fn apply_scene_change(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    ndi_sources: &NdiSourceMap,
    obs: &ObsShared,
    event_tx: &broadcast::Sender<ObsEvent>,
    scene_name: String,
    ticket: u64,
) {
    let sources = ndi_sources.read().await;
    let lookup = check_scene_items(write, dispatcher, &scene_name, &sources).await;
    drop(sources);

    let active_ids = match lookup {
        Ok(ids) => ids,
        Err(e) => {
            let written = obs
                .update_scene(ticket, |s| {
                    let repeated = s.lookup_failed.as_deref() == Some(scene_name.as_str());
                    s.current_scene = Some(scene_name.clone());
                    s.lookup_failed = Some(scene_name.clone());
                    repeated
                })
                .await;
            match written {
                None => debug!(
                    scene = %scene_name,
                    error = %e,
                    "obs: a newer scene lookup already answered — this failed one is dropped"
                ),
                Some(true) => {
                    debug!(scene = %scene_name, error = %e, "obs: the scene's playlist lookup failed again")
                }
                Some(false) => warn!(
                    scene = %scene_name,
                    error = %e,
                    "obs: looking up the scene's playlists failed — keeping the previous ones; the scene poll looks it up again"
                ),
            }
            return;
        }
    };

    let written = obs
        .update_scene(ticket, |s| {
            let repaired = s.lookup_failed.take().as_deref() == Some(scene_name.as_str());
            s.current_scene = Some(scene_name.clone());
            s.active_playlist_ids = active_ids.clone();
            let _ = event_tx.send(ObsEvent::SceneChanged {
                scene_name: scene_name.clone(),
                active_playlist_ids: active_ids.clone(),
            });
            repaired
        })
        .await;
    match written {
        None => debug!(
            scene = %scene_name,
            "obs: a newer scene lookup already answered — this answer is dropped"
        ),
        Some(true) => info!(
            scene = %scene_name,
            playlists = ?active_ids,
            "obs: the scene's playlist lookup answered again"
        ),
        Some(false) => {}
    }
}

/// Check which NDI sources are present in a given scene.
///
/// Sends `GetSceneItemList` for the scene, checks each item name against the
/// NDI source map. Recurses into nested scenes / group sources. A lookup that
/// failed anywhere is an `Err` (#218) — never a partial or empty set — except
/// a nested item cg OBS REFUSES to list: obs-websocket 5 answers
/// `GetSceneItemList` for a group with 602 "Is group", so a nested refusal
/// adds nothing, as it always did.
pub async fn check_scene_items(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    scene_name: &str,
    ndi_sources: &HashMap<String, i64>,
) -> Result<HashSet<i64>, LookupError> {
    let mut active_ids = HashSet::new();
    check_scene_items_recursive(
        write,
        dispatcher,
        scene_name,
        ndi_sources,
        &mut active_ids,
        0,
    )
    .await?;
    Ok(active_ids)
}

/// Maximum recursion depth for nested scenes to prevent infinite loops.
const MAX_RECURSION_DEPTH: u32 = 5;

/// The items of an op=7 `GetSceneItemList` reply, or why there are none to
/// read: a refusal (`requestStatus.result` false) or no `sceneItems` list.
pub(crate) fn scene_items_from_reply(reply: &Value) -> Result<&Vec<Value>, LookupError> {
    let d = &reply["d"];
    if d["requestStatus"]["result"].as_bool() == Some(false) {
        return Err(LookupError::Refused {
            code: d["requestStatus"]["code"].as_u64().unwrap_or(0),
            comment: d["requestStatus"]["comment"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        });
    }
    d["responseData"]["sceneItems"]
        .as_array()
        .ok_or(LookupError::NoSceneItems)
}

async fn check_scene_items_recursive(
    write: &SharedWrite,
    dispatcher: &Dispatcher,
    scene_name: &str,
    ndi_sources: &HashMap<String, i64>,
    active_ids: &mut HashSet<i64>,
    depth: u32,
) -> Result<(), LookupError> {
    if depth >= MAX_RECURSION_DEPTH {
        warn!("max scene recursion depth reached for '{scene_name}'");
        return Ok(());
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let req = get_scene_items_request(&request_id, scene_name);
    let reply = dispatcher
        .send_and_await(
            write,
            request_id,
            Message::Text(req.to_string().into()),
            DEFAULT_RESPONSE_TIMEOUT,
        )
        .await?;

    let scene_items = match scene_items_from_reply(&reply) {
        Ok(items) => items,
        Err(LookupError::Refused { code, comment }) if depth > 0 => {
            debug!(
                scene = scene_name,
                code,
                comment = %comment,
                "obs: a nested scene item's list was refused — it adds no playlist"
            );
            return Ok(());
        }
        Err(e) => return Err(e),
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
            .await?;
        }
    }
    Ok(())
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

    // ---- #218: a failed lookup is an Err, never an empty set ----

    #[test]
    fn a_listed_scene_yields_its_items_even_an_empty_list() {
        let listed = serde_json::json!({
            "op": 7,
            "d": {
                "requestStatus": { "result": true, "code": 100 },
                "responseData": { "sceneItems": [ { "sourceName": "sp-fast_video" } ] },
            }
        });
        let items = scene_items_from_reply(&listed).expect("listed");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["sourceName"], "sp-fast_video");
        let empty = serde_json::json!({
            "d": {
                "requestStatus": { "result": true, "code": 100 },
                "responseData": { "sceneItems": [] },
            }
        });
        assert_eq!(
            scene_items_from_reply(&empty).map(Vec::len),
            Ok(0),
            "a scene with no items is a real (empty) answer"
        );
    }

    #[test]
    fn an_answer_without_scene_items_is_a_failed_lookup() {
        let no_items = serde_json::json!({
            "d": {
                "requestStatus": { "result": true, "code": 100 },
                "responseData": {},
            }
        });
        assert_eq!(
            scene_items_from_reply(&no_items),
            Err(LookupError::NoSceneItems)
        );
        assert_eq!(
            scene_items_from_reply(&serde_json::json!({})),
            Err(LookupError::NoSceneItems),
            "no `d` at all"
        );
    }

    #[test]
    fn a_refused_request_is_a_failed_lookup_with_its_code() {
        // obs-websocket 5's answer to GetSceneItemList for a group.
        let refused = serde_json::json!({
            "d": {
                "requestStatus": {
                    "result": false,
                    "code": 602,
                    "comment": "The specified source is not a scene. (Is group)",
                },
            }
        });
        assert_eq!(
            scene_items_from_reply(&refused),
            Err(LookupError::Refused {
                code: 602,
                comment: "The specified source is not a scene. (Is group)".to_string(),
            })
        );
        let bare = serde_json::json!({ "d": { "requestStatus": { "result": false } } });
        assert_eq!(
            scene_items_from_reply(&bare),
            Err(LookupError::Refused {
                code: 0,
                comment: String::new(),
            })
        );
    }

    #[test]
    fn a_dispatcher_failure_is_a_failed_lookup_and_every_failure_reads_in_a_log() {
        assert_eq!(
            LookupError::from(DispatcherError::Timeout),
            LookupError::Timeout
        );
        assert_eq!(
            LookupError::from(DispatcherError::Closed),
            LookupError::Closed
        );
        assert_eq!(LookupError::Timeout.to_string(), "no answer in time");
        assert_eq!(
            LookupError::Refused {
                code: 600,
                comment: "No source".to_string()
            }
            .to_string(),
            "refused (600: No source)"
        );
        assert_eq!(
            LookupError::NoSceneItems.to_string(),
            "answered without a sceneItems list"
        );
        assert_eq!(
            LookupError::Closed.to_string(),
            "the OBS connection closed before the answer"
        );
    }
}
