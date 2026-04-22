//! `PresenterPayload` — request body for `PUT /api/stage` on the Presenter
//! stage-display API. Matches the spec exactly:
//!   - field names serialize camelCase: `currentText`, `nextText`, etc.
//!   - missing-on-the-wire fields default to "" server-side (not displayed)
//!
//! `currentGroup` / `nextGroup` are intentionally omitted — SongPlayer has
//! no notion of worship-team groups today. Follow-up can add them via
//! per-playlist or per-line metadata when bands start asking for colour
//! bars on the stage display.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PresenterPayload {
    pub current_text: String,
    pub next_text: String,
    pub current_song: String,
    pub next_song: String,
}

impl PresenterPayload {
    /// Four empty strings — clears the stage display on the Presenter side.
    pub fn empty() -> Self {
        Self {
            current_text: String::new(),
            next_text: String::new(),
            current_song: String::new(),
            next_song: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_camel_case_keys_matching_api_spec() {
        let p = PresenterPayload {
            current_text: "Haleluja, haleluja".to_string(),
            next_text: "Spievajte Hospodinovi".to_string(),
            current_song: "Haleluja".to_string(),
            next_song: "Spievajte".to_string(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["currentText"], "Haleluja, haleluja");
        assert_eq!(json["nextText"], "Spievajte Hospodinovi");
        assert_eq!(json["currentSong"], "Haleluja");
        assert_eq!(json["nextSong"], "Spievajte");
    }

    #[test]
    fn does_not_serialize_group_fields() {
        // currentGroup / nextGroup are intentionally NOT on this struct.
        // The Presenter API treats missing fields as empty, not displayed —
        // which is what we want until SongPlayer learns about groups.
        let p = PresenterPayload::empty();
        let json = serde_json::to_value(&p).unwrap();
        let obj = json.as_object().expect("object");
        assert!(
            obj.get("currentGroup").is_none(),
            "currentGroup must NOT appear in the serialized body (got: {json})"
        );
        assert!(
            obj.get("nextGroup").is_none(),
            "nextGroup must NOT appear in the serialized body (got: {json})"
        );
    }

    #[test]
    fn empty_returns_four_empty_strings() {
        let p = PresenterPayload::empty();
        assert!(p.current_text.is_empty());
        assert!(p.next_text.is_empty());
        assert!(p.current_song.is_empty());
        assert!(p.next_song.is_empty());
    }
}
