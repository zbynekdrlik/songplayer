//! Database models mirroring the SQLite schema.

use serde::{Deserialize, Serialize};

/// A playlist being tracked. `kind = "youtube"` is the default YouTube-backed
/// kind; `kind = "custom"` is an operator-curated set list used by the Live
/// dashboard. `current_position` is only meaningful for custom playlists
/// (tracks which set-list item was last played).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub youtube_url: String,
    #[serde(default)]
    pub ndi_output_name: String,
    #[serde(default)]
    pub playback_mode: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default = "default_true")]
    pub karaoke_enabled: bool,
    #[serde(default = "default_kind_youtube")]
    pub kind: String,
    #[serde(default)]
    pub current_position: i64,
}

fn default_true() -> bool {
    true
}

fn default_kind_youtube() -> String {
    "youtube".to_string()
}

impl Default for Playlist {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            youtube_url: String::new(),
            ndi_output_name: String::new(),
            playback_mode: String::new(),
            is_active: false,
            created_at: None,
            updated_at: None,
            karaoke_enabled: true,
            kind: default_kind_youtube(),
            current_position: 0,
        }
    }
}

/// A single video within a playlist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Video {
    pub id: i64,
    pub playlist_id: i64,
    pub youtube_id: String,
    pub title: String,
    pub song: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: Option<i64>,
    pub cached: bool,
    pub normalized: bool,
    pub gemini_failed: bool,
    /// V14: suppress Resolume EN subs for songs with baked-in lyrics in the
    /// video. SK subs + Presenter current_text remain unaffected. Default 0.
    #[serde(default)]
    pub suppress_resolume_en: bool,
}

/// A record of a video that was played.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlayHistoryEntry {
    pub id: i64,
    pub video_id: i64,
    pub playlist_id: i64,
    pub played_at: String,
}

/// A key-value setting stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Setting {
    pub key: String,
    pub value: String,
}

/// A Resolume Arena host for NDI output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolumeHost {
    pub id: i64,
    #[serde(default)]
    pub label: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub is_enabled: bool,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// Maps a playlist to a specific Resolume clip slot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolumeClipMapping {
    pub id: i64,
    pub resolume_host_id: i64,
    pub playlist_id: i64,
    pub layer: u32,
    pub column: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_karaoke_enabled_defaults_to_true() {
        let json = r#"{"id": 1, "name": "test", "youtube_url": "url", "ndi_output_name": "", "playback_mode": "continuous", "is_active": true}"#;
        let p: Playlist = serde_json::from_str(json).unwrap();
        assert!(p.karaoke_enabled);
    }

    #[test]
    fn playlist_default_kind_is_youtube() {
        let p = Playlist::default();
        assert_eq!(p.kind, "youtube");
        assert_eq!(p.current_position, 0);
    }

    #[test]
    fn playlist_deserialises_kind_and_current_position() {
        let json = r#"{
            "id": 7, "name": "ytlive", "youtube_url": "",
            "ndi_output_name": "SP-live", "playback_mode": "continuous",
            "is_active": true, "kind": "custom", "current_position": 3
        }"#;
        let p: Playlist = serde_json::from_str(json).unwrap();
        assert_eq!(p.kind, "custom");
        assert_eq!(p.current_position, 3);
    }

    #[test]
    fn playlist_missing_kind_defaults_to_youtube_via_serde() {
        let json = r#"{"id": 1, "name": "x", "youtube_url": "u"}"#;
        let p: Playlist = serde_json::from_str(json).unwrap();
        assert_eq!(p.kind, "youtube");
        assert_eq!(p.current_position, 0);
    }
}
