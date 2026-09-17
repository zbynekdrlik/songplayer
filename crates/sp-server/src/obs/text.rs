//! Text source control — build OBS WebSocket v5 request messages.

/// Build a `SetInputSettings` request to update a text source.
pub fn set_text_request(request_id: &str, source_name: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "SetInputSettings",
            "requestId": request_id,
            "requestData": {
                "inputName": source_name,
                "inputSettings": { "text": text }
            }
        }
    })
}

/// Build a `GetCurrentProgramScene` request.
pub fn get_current_scene_request(request_id: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetCurrentProgramScene",
            "requestId": request_id
        }
    })
}

/// Build a `GetSceneItemList` request for a given scene.
pub fn get_scene_items_request(request_id: &str, scene_name: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetSceneItemList",
            "requestId": request_id,
            "requestData": {
                "sceneName": scene_name
            }
        }
    })
}

/// Build a `SetInputSettings` request that writes an NDI input's
/// `ndi_source_name` (#127 receiver recovery). Used to clear (`""`) then
/// restore the field so DistroAV re-runs discovery for a stranded receiver.
/// The default merge semantics (`overlay` unset ⇒ merge) leave every other
/// input setting untouched.
pub fn set_ndi_source_name_request(
    request_id: &str,
    input_name: &str,
    ndi_source_name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "SetInputSettings",
            "requestId": request_id,
            "requestData": {
                "inputName": input_name,
                "inputSettings": { "ndi_source_name": ndi_source_name }
            }
        }
    })
}

/// Build a `GetInputList` request filtered to NDI source inputs.
pub fn get_input_list_request(request_id: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetInputList",
            "requestId": request_id,
            "requestData": { "inputKind": "ndi_source" }
        }
    })
}

/// Build a `GetInputSettings` request for a specific input.
pub fn get_input_settings_request(request_id: &str, input_name: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetInputSettings",
            "requestId": request_id,
            "requestData": { "inputName": input_name }
        }
    })
}

// ---------------------------------------------------------------------------
// #173 round 2: escalation-ladder I/O builders (obs-websocket v5.x).
//
// The dark-wall recovery ladder (obs/ndi_recovery.rs) needs three OBS remedies
// beyond the round-1 clear+restore nudge: toggle the input's scene item
// off→on (rung 1) and remove+recreate the input keeping its scene-item
// transform/index (rung 2). These pure builders back that executor
// (obs/ndi_recovery_io.rs); field names verified against the obs-websocket 5.x
// protocol (`CreateInput`/`SetSceneItemEnabled`/`SetSceneItemTransform`/
// `RemoveInput`/`GetSceneItemTransform`/`SetSceneItemIndex`).
// ---------------------------------------------------------------------------

/// Build a `GetSceneList` request (no requestData). Used to find which scene an
/// NDI input's scene item lives in before toggling / recreating it.
pub fn get_scene_list_request(request_id: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetSceneList",
            "requestId": request_id
        }
    })
}

/// Build a `SetSceneItemEnabled` request — the rung-1 remedy (toggle a source's
/// scene item off then on so DistroAV tears down and recreates the receiver).
pub fn set_scene_item_enabled_request(
    request_id: &str,
    scene_name: &str,
    scene_item_id: i64,
    enabled: bool,
) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "SetSceneItemEnabled",
            "requestId": request_id,
            "requestData": {
                "sceneName": scene_name,
                "sceneItemId": scene_item_id,
                "sceneItemEnabled": enabled
            }
        }
    })
}

/// Build a `GetSceneItemTransform` request — read the transform/crop so the
/// rung-2 recreate can restore it after `RemoveInput` + `CreateInput`.
pub fn get_scene_item_transform_request(
    request_id: &str,
    scene_name: &str,
    scene_item_id: i64,
) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetSceneItemTransform",
            "requestId": request_id,
            "requestData": {
                "sceneName": scene_name,
                "sceneItemId": scene_item_id
            }
        }
    })
}

/// Build a `SetSceneItemTransform` request — restore the saved transform onto
/// the recreated scene item.
pub fn set_scene_item_transform_request(
    request_id: &str,
    scene_name: &str,
    scene_item_id: i64,
    transform: &serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "SetSceneItemTransform",
            "requestId": request_id,
            "requestData": {
                "sceneName": scene_name,
                "sceneItemId": scene_item_id,
                "sceneItemTransform": transform
            }
        }
    })
}

/// Build a `SetSceneItemIndex` request — restore the recreated item's z-order
/// (a fresh `CreateInput` adds the item at the top of the scene).
pub fn set_scene_item_index_request(
    request_id: &str,
    scene_name: &str,
    scene_item_id: i64,
    scene_item_index: i64,
) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "SetSceneItemIndex",
            "requestId": request_id,
            "requestData": {
                "sceneName": scene_name,
                "sceneItemId": scene_item_id,
                "sceneItemIndex": scene_item_index
            }
        }
    })
}

/// Build a `RemoveInput` request — the first half of the rung-2 recreate.
/// Removes the input and every scene item referencing it.
pub fn remove_input_request(request_id: &str, input_name: &str) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "RemoveInput",
            "requestId": request_id,
            "requestData": { "inputName": input_name }
        }
    })
}

/// Build a `CreateInput` request — the second half of the rung-2 recreate.
/// Re-adds the NDI input to `scene_name` with the identical `inputSettings`
/// (the ADVERTISED, case-correct `ndi_source_name`) and returns a fresh
/// `sceneItemId`.
pub fn create_input_request(
    request_id: &str,
    scene_name: &str,
    input_name: &str,
    input_kind: &str,
    input_settings: &serde_json::Value,
    enabled: bool,
) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "CreateInput",
            "requestId": request_id,
            "requestData": {
                "sceneName": scene_name,
                "inputName": input_name,
                "inputKind": input_kind,
                "inputSettings": input_settings,
                "sceneItemEnabled": enabled
            }
        }
    })
}

/// Build a `GetSceneItemEnabled` request — the rung-1 read-back (#173 round 3).
/// After a `SetSceneItemEnabled` OFF→ON toggle, the executor reads this back and,
/// if the item is still disabled (an operator hid it), re-enables it so the
/// ladder never leaves an on-program item hidden on a dark wall.
pub fn get_scene_item_enabled_request(
    request_id: &str,
    scene_name: &str,
    scene_item_id: i64,
) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "GetSceneItemEnabled",
            "requestId": request_id,
            "requestData": {
                "sceneName": scene_name,
                "sceneItemId": scene_item_id
            }
        }
    })
}

/// Build a `SetInputName` request — the final step of the create-first rung-2
/// recreate (#173 round 3): rename the temporary `<input>_recover` input back to
/// the original name AFTER the old input has been removed (so the name is free).
pub fn set_input_name_request(
    request_id: &str,
    input_name: &str,
    new_input_name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "op": 6,
        "d": {
            "requestType": "SetInputName",
            "requestId": request_id,
            "requestData": {
                "inputName": input_name,
                "newInputName": new_input_name
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_scene_item_enabled_request_structure() {
        let req = get_scene_item_enabled_request("req-e", "sp-youth", 2);
        assert_eq!(req["d"]["requestType"], "GetSceneItemEnabled");
        assert_eq!(req["d"]["requestData"]["sceneName"], "sp-youth");
        assert_eq!(req["d"]["requestData"]["sceneItemId"], 2);
    }

    #[test]
    fn set_input_name_request_structure() {
        let req = set_input_name_request("req-r", "sp-youth_video_recover", "sp-youth_video");
        assert_eq!(req["d"]["requestType"], "SetInputName");
        assert_eq!(
            req["d"]["requestData"]["inputName"],
            "sp-youth_video_recover"
        );
        assert_eq!(req["d"]["requestData"]["newInputName"], "sp-youth_video");
    }

    #[test]
    fn test_set_text_request_structure() {
        let req = set_text_request("req-1", "title_source", "Hello World");

        assert_eq!(req["op"], 6);
        assert_eq!(req["d"]["requestType"], "SetInputSettings");
        assert_eq!(req["d"]["requestId"], "req-1");
        assert_eq!(req["d"]["requestData"]["inputName"], "title_source");
        assert_eq!(
            req["d"]["requestData"]["inputSettings"]["text"],
            "Hello World"
        );
    }

    #[test]
    fn test_set_text_request_empty_text() {
        let req = set_text_request("req-2", "source", "");
        assert_eq!(req["d"]["requestData"]["inputSettings"]["text"], "");
    }

    #[test]
    fn test_set_text_request_special_characters() {
        let req = set_text_request("req-3", "source", "Line 1\nLine 2\t\"quoted\"");
        assert_eq!(
            req["d"]["requestData"]["inputSettings"]["text"],
            "Line 1\nLine 2\t\"quoted\""
        );
    }

    #[test]
    fn test_get_current_scene_request_structure() {
        let req = get_current_scene_request("scene-req-1");

        assert_eq!(req["op"], 6);
        assert_eq!(req["d"]["requestType"], "GetCurrentProgramScene");
        assert_eq!(req["d"]["requestId"], "scene-req-1");
        // Should not have requestData.
        assert!(req["d"]["requestData"].is_null());
    }

    #[test]
    fn test_get_scene_items_request_structure() {
        let req = get_scene_items_request("items-req-1", "Main Scene");

        assert_eq!(req["op"], 6);
        assert_eq!(req["d"]["requestType"], "GetSceneItemList");
        assert_eq!(req["d"]["requestId"], "items-req-1");
        assert_eq!(req["d"]["requestData"]["sceneName"], "Main Scene");
    }

    #[test]
    fn test_get_input_list_request_structure() {
        let req = get_input_list_request("inputs-req-1");
        assert_eq!(req["op"], 6);
        assert_eq!(req["d"]["requestType"], "GetInputList");
        assert_eq!(req["d"]["requestId"], "inputs-req-1");
        assert_eq!(req["d"]["requestData"]["inputKind"], "ndi_source");
    }

    #[test]
    fn test_set_ndi_source_name_request_structure() {
        let req = set_ndi_source_name_request("nudge-1", "sp-slow_video", "");
        assert_eq!(req["op"], 6);
        assert_eq!(req["d"]["requestType"], "SetInputSettings");
        assert_eq!(req["d"]["requestId"], "nudge-1");
        assert_eq!(req["d"]["requestData"]["inputName"], "sp-slow_video");
        // Clearing writes an empty string (the proven force-rediscovery step).
        assert_eq!(
            req["d"]["requestData"]["inputSettings"]["ndi_source_name"],
            ""
        );
        // Restore writes the full network-visible sender name back.
        let restore =
            set_ndi_source_name_request("nudge-2", "sp-slow_video", "RESOLUME-SNV (SP-slow)");
        assert_eq!(
            restore["d"]["requestData"]["inputSettings"]["ndi_source_name"],
            "RESOLUME-SNV (SP-slow)"
        );
    }

    #[test]
    fn test_get_input_settings_request_structure() {
        let req = get_input_settings_request("settings-req-1", "sp-fast_video");
        assert_eq!(req["op"], 6);
        assert_eq!(req["d"]["requestType"], "GetInputSettings");
        assert_eq!(req["d"]["requestId"], "settings-req-1");
        assert_eq!(req["d"]["requestData"]["inputName"], "sp-fast_video");
    }

    #[test]
    fn test_get_scene_list_request_structure() {
        let req = get_scene_list_request("scenes-1");
        assert_eq!(req["op"], 6);
        assert_eq!(req["d"]["requestType"], "GetSceneList");
        assert_eq!(req["d"]["requestId"], "scenes-1");
        assert!(req["d"]["requestData"].is_null());
    }

    #[test]
    fn test_set_scene_item_enabled_request_structure() {
        let off = set_scene_item_enabled_request("toggle-off", "sp-slow", 1, false);
        assert_eq!(off["op"], 6);
        assert_eq!(off["d"]["requestType"], "SetSceneItemEnabled");
        assert_eq!(off["d"]["requestData"]["sceneName"], "sp-slow");
        assert_eq!(off["d"]["requestData"]["sceneItemId"], 1);
        assert_eq!(off["d"]["requestData"]["sceneItemEnabled"], false);
        let on = set_scene_item_enabled_request("toggle-on", "sp-slow", 1, true);
        assert_eq!(on["d"]["requestData"]["sceneItemEnabled"], true);
    }

    #[test]
    fn test_get_scene_item_transform_request_structure() {
        let req = get_scene_item_transform_request("xf-get", "sp-fast", 7);
        assert_eq!(req["d"]["requestType"], "GetSceneItemTransform");
        assert_eq!(req["d"]["requestData"]["sceneName"], "sp-fast");
        assert_eq!(req["d"]["requestData"]["sceneItemId"], 7);
    }

    #[test]
    fn test_set_scene_item_transform_request_structure() {
        let xf = serde_json::json!({ "positionX": 0, "scaleX": 1.0, "boundsWidth": 1920 });
        let req = set_scene_item_transform_request("xf-set", "sp-fast", 7, &xf);
        assert_eq!(req["d"]["requestType"], "SetSceneItemTransform");
        assert_eq!(req["d"]["requestData"]["sceneName"], "sp-fast");
        assert_eq!(req["d"]["requestData"]["sceneItemId"], 7);
        assert_eq!(
            req["d"]["requestData"]["sceneItemTransform"]["boundsWidth"],
            1920
        );
    }

    #[test]
    fn test_set_scene_item_index_request_structure() {
        let req = set_scene_item_index_request("idx", "sp-fast", 7, 2);
        assert_eq!(req["d"]["requestType"], "SetSceneItemIndex");
        assert_eq!(req["d"]["requestData"]["sceneName"], "sp-fast");
        assert_eq!(req["d"]["requestData"]["sceneItemId"], 7);
        assert_eq!(req["d"]["requestData"]["sceneItemIndex"], 2);
    }

    #[test]
    fn test_remove_input_request_structure() {
        let req = remove_input_request("rm", "sp-youth_video");
        assert_eq!(req["d"]["requestType"], "RemoveInput");
        assert_eq!(req["d"]["requestData"]["inputName"], "sp-youth_video");
    }

    #[test]
    fn test_create_input_request_structure() {
        let settings = serde_json::json!({ "ndi_source_name": "RESOLUME-SNV (SP-youth)" });
        let req = create_input_request(
            "mk",
            "sp-youth",
            "sp-youth_video",
            "ndi_source",
            &settings,
            true,
        );
        assert_eq!(req["d"]["requestType"], "CreateInput");
        assert_eq!(req["d"]["requestData"]["sceneName"], "sp-youth");
        assert_eq!(req["d"]["requestData"]["inputName"], "sp-youth_video");
        assert_eq!(req["d"]["requestData"]["inputKind"], "ndi_source");
        assert_eq!(req["d"]["requestData"]["sceneItemEnabled"], true);
        assert_eq!(
            req["d"]["requestData"]["inputSettings"]["ndi_source_name"],
            "RESOLUME-SNV (SP-youth)"
        );
    }

    #[test]
    fn test_all_requests_are_op_6() {
        let r1 = set_text_request("a", "b", "c");
        let r2 = get_current_scene_request("a");
        let r3 = get_scene_items_request("a", "b");

        assert_eq!(r1["op"], 6);
        assert_eq!(r2["op"], 6);
        assert_eq!(r3["op"], 6);
    }

    #[test]
    fn test_requests_are_valid_json() {
        let r1 = set_text_request("a", "b", "c");
        let r2 = get_current_scene_request("a");
        let r3 = get_scene_items_request("a", "b");

        // All should serialize to valid JSON strings.
        assert!(serde_json::to_string(&r1).is_ok());
        assert!(serde_json::to_string(&r2).is_ok());
        assert!(serde_json::to_string(&r3).is_ok());
    }
}
