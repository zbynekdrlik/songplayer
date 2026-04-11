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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_get_input_settings_request_structure() {
        let req = get_input_settings_request("settings-req-1", "sp-fast_video");
        assert_eq!(req["op"], 6);
        assert_eq!(req["d"]["requestType"], "GetInputSettings");
        assert_eq!(req["d"]["requestId"], "settings-req-1");
        assert_eq!(req["d"]["requestData"]["inputName"], "sp-fast_video");
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
