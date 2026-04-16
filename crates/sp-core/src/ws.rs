//! WebSocket message types for the dashboard ↔ server protocol.

use serde::{Deserialize, Serialize};

use crate::playback::{PlaybackMode, PlaybackState};

/// State of a song currently being processed by the lyrics pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LyricsProcessingState {
    pub video_id: i64,
    pub youtube_id: String,
    pub song: String,
    pub artist: String,
    pub stage: String,
    pub provider: Option<String>,
    pub started_at_unix_ms: i64,
}

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
    LyricsQueueUpdate {
        bucket0_count: i64,
        bucket1_count: i64,
        bucket2_count: i64,
        pipeline_version: u32,
        processing: Option<LyricsProcessingState>,
    },
    LyricsProcessingStage {
        video_id: i64,
        youtube_id: String,
        stage: String,
        provider: Option<String>,
    },
    LyricsCompleted {
        video_id: i64,
        youtube_id: String,
        source: String,
        quality_score: f32,
        provider_count: u8,
        duration_ms: u64,
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

    #[test]
    fn lyrics_queue_update_roundtrip() {
        let msg = ServerMsg::LyricsQueueUpdate {
            bucket0_count: 3,
            bucket1_count: 12,
            bucket2_count: 187,
            pipeline_version: 2,
            processing: Some(LyricsProcessingState {
                video_id: 42,
                youtube_id: "abc".into(),
                song: "Hello".into(),
                artist: "Adele".into(),
                stage: "aligning".into(),
                provider: Some("qwen3".into()),
                started_at_unix_ms: 1718380800000,
            }),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn lyrics_processing_stage_roundtrip() {
        let msg = ServerMsg::LyricsProcessingStage {
            video_id: 42,
            youtube_id: "abc".into(),
            stage: "text_merge".into(),
            provider: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn lyrics_completed_roundtrip() {
        let msg = ServerMsg::LyricsCompleted {
            video_id: 42,
            youtube_id: "abc".into(),
            source: "ensemble:qwen3+autosub".into(),
            quality_score: 0.82,
            provider_count: 2,
            duration_ms: 330_000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: ServerMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, decoded);
    }
}
