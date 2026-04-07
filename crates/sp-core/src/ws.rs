//! WebSocket message types for the dashboard ↔ server protocol.

use serde::{Deserialize, Serialize};

use crate::playback::{PlaybackMode, PlaybackState};

/// Messages sent from the UI client to the server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum ClientMsg {
    Play {
        playlist_id: i64,
    },
    Pause {
        playlist_id: i64,
    },
    Skip {
        playlist_id: i64,
    },
    Previous {
        playlist_id: i64,
    },
    SetMode {
        playlist_id: i64,
        mode: PlaybackMode,
    },
    SyncPlaylist {
        playlist_id: i64,
    },
    Ping,
}

/// Messages sent from the server to UI clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data")]
pub enum ServerMsg {
    NowPlaying {
        playlist_id: i64,
        video_id: i64,
        song: String,
        artist: String,
        position_ms: u64,
        duration_ms: u64,
    },
    PlaybackStateChanged {
        playlist_id: i64,
        state: PlaybackState,
        mode: PlaybackMode,
    },
    QueueUpdate {
        playlist_id: i64,
        video_count: u32,
        cached_count: u32,
    },
    DownloadProgress {
        playlist_id: i64,
        youtube_id: String,
        title: String,
        progress_pct: f32,
        stage: String,
    },
    ObsStatus {
        connected: bool,
        active_scene: Option<String>,
    },
    ResolumeStatus {
        host_id: i64,
        connected: bool,
    },
    ToolsStatus {
        ytdlp_available: bool,
        ffmpeg_available: bool,
        ytdlp_version: Option<String>,
    },
    Error {
        message: String,
    },
    Pong,
}
