//! Database models mirroring the SQLite schema.

use serde::{Deserialize, Serialize};

/// A YouTube playlist being tracked.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Playlist {
    pub id: i64,
    pub youtube_playlist_id: String,
    pub name: String,
    pub enabled: bool,
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
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub enabled: bool,
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
