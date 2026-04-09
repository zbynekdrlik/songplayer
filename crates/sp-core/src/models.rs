//! Database models mirroring the SQLite schema.

use serde::{Deserialize, Serialize};

/// A YouTube playlist being tracked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub youtube_url: String,
    #[serde(default)]
    pub ndi_output_name: String,
    #[serde(default)]
    pub obs_text_source: Option<String>,
    #[serde(default)]
    pub playback_mode: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
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
