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
    LyricsUpdate {
        playlist_id: i64,
        line_en: Option<String>,
        line_sk: Option<String>,
        prev_line_en: Option<String>,
        next_line_en: Option<String>,
        active_word_index: Option<usize>,
        word_count: Option<usize>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lyrics_update_roundtrip_all_fields() {
        let msg = ServerMsg::LyricsUpdate {
            playlist_id: 42,
            line_en: Some("Hello world".to_string()),
            line_sk: Some("Ahoj svet".to_string()),
            prev_line_en: Some("Previous line".to_string()),
            next_line_en: Some("Next line".to_string()),
            active_word_index: Some(1),
            word_count: Some(2),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: ServerMsg = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn lyrics_update_roundtrip_all_none() {
        let msg = ServerMsg::LyricsUpdate {
            playlist_id: 1,
            line_en: None,
            line_sk: None,
            prev_line_en: None,
            next_line_en: None,
            active_word_index: None,
            word_count: None,
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let decoded: ServerMsg = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(msg, decoded);
    }
}
