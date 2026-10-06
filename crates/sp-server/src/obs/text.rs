//! Text source control — build OBS WebSocket v5 request messages.
//!
//! #221 lane 3 deleted the #173 ladder's request builders (scene items, input
//! settings, create / remove / rename an input) with the per-playlist NDI
//! senders whose cg OBS inputs the ladder nudged: what is left is the title
//! text.

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
    fn test_set_text_request_is_valid_json() {
        let r = set_text_request("a", "b", "c");
        assert!(serde_json::to_string(&r).is_ok());
    }
}
