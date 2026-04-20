//! SongPlayer shared types and models.
//!
//! This crate is WASM-safe — no OS-specific dependencies.

pub mod config;
pub mod lyrics;
pub mod metadata;
pub mod models;
pub mod playback;
pub mod ws;

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PlaybackMode ----

    #[test]
    fn playback_mode_default_is_continuous() {
        assert_eq!(
            playback::PlaybackMode::default(),
            playback::PlaybackMode::Continuous
        );
    }

    #[test]
    fn playback_mode_as_str() {
        assert_eq!(playback::PlaybackMode::Continuous.as_str(), "continuous");
        assert_eq!(playback::PlaybackMode::Single.as_str(), "single");
        assert_eq!(playback::PlaybackMode::Loop.as_str(), "loop");
    }

    #[test]
    fn playback_mode_from_str_lossy_valid() {
        assert_eq!(
            playback::PlaybackMode::from_str_lossy("continuous"),
            playback::PlaybackMode::Continuous,
        );
        assert_eq!(
            playback::PlaybackMode::from_str_lossy("SINGLE"),
            playback::PlaybackMode::Single,
        );
        assert_eq!(
            playback::PlaybackMode::from_str_lossy("Loop"),
            playback::PlaybackMode::Loop,
        );
    }

    #[test]
    fn playback_mode_from_str_lossy_garbage_returns_continuous() {
        assert_eq!(
            playback::PlaybackMode::from_str_lossy("garbage"),
            playback::PlaybackMode::Continuous,
        );
        assert_eq!(
            playback::PlaybackMode::from_str_lossy(""),
            playback::PlaybackMode::Continuous,
        );
        assert_eq!(
            playback::PlaybackMode::from_str_lossy("repeat"),
            playback::PlaybackMode::Continuous,
        );
    }

    #[test]
    fn playback_mode_serde_roundtrip() {
        for mode in [
            playback::PlaybackMode::Continuous,
            playback::PlaybackMode::Single,
            playback::PlaybackMode::Loop,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: playback::PlaybackMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
    }

    // ---- PlaybackState ----

    #[test]
    fn playback_state_default_is_idle() {
        assert_eq!(
            playback::PlaybackState::default(),
            playback::PlaybackState::Idle
        );
    }

    #[test]
    fn playback_state_serde_roundtrip() {
        for state in [
            playback::PlaybackState::Idle,
            playback::PlaybackState::WaitingForScene,
            playback::PlaybackState::Playing,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: playback::PlaybackState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    // ---- MetadataSource ----

    #[test]
    fn metadata_source_as_str() {
        assert_eq!(metadata::MetadataSource::Gemini.as_str(), "gemini");
        assert_eq!(metadata::MetadataSource::Regex.as_str(), "regex");
    }

    #[test]
    fn metadata_source_serde_roundtrip() {
        for src in [
            metadata::MetadataSource::Gemini,
            metadata::MetadataSource::Regex,
        ] {
            let json = serde_json::to_string(&src).unwrap();
            let back: metadata::MetadataSource = serde_json::from_str(&json).unwrap();
            assert_eq!(src, back);
        }
    }

    // ---- VideoMetadata ----

    #[test]
    fn video_metadata_serde_roundtrip() {
        let meta = metadata::VideoMetadata {
            song: "Bohemian Rhapsody".into(),
            artist: "Queen".into(),
            source: metadata::MetadataSource::Gemini,
            gemini_failed: false,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: metadata::VideoMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(meta, back);
    }

    // ---- Models ----

    #[test]
    fn playlist_serde_roundtrip() {
        let p = models::Playlist {
            id: 1,
            name: "Test".into(),
            youtube_url: "https://youtube.com/playlist?list=PLxyz".into(),
            ndi_output_name: "SP-test".into(),
            playback_mode: "continuous".into(),
            is_active: true,
            created_at: None,
            updated_at: None,
            karaoke_enabled: true,
            kind: "youtube".into(),
            current_position: 0,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: models::Playlist = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn video_serde_roundtrip() {
        let v = models::Video {
            id: 42,
            playlist_id: 1,
            youtube_id: "dQw4w9WgXcQ".into(),
            title: "Never Gonna Give You Up".into(),
            song: Some("Never Gonna Give You Up".into()),
            artist: Some("Rick Astley".into()),
            duration_ms: Some(213000),
            cached: true,
            normalized: true,
            gemini_failed: false,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: models::Video = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn play_history_entry_serde_roundtrip() {
        let e = models::PlayHistoryEntry {
            id: 1,
            video_id: 42,
            playlist_id: 1,
            played_at: "2026-04-06T12:00:00Z".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: models::PlayHistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn setting_serde_roundtrip() {
        let s = models::Setting {
            key: "obs_websocket_url".into(),
            value: "ws://127.0.0.1:4455".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: models::Setting = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn resolume_host_serde_roundtrip() {
        let h = models::ResolumeHost {
            id: 1,
            label: "Main".into(),
            host: "192.168.1.10".into(),
            port: 7000,
            is_enabled: true,
            created_at: None,
        };
        let json = serde_json::to_string(&h).unwrap();
        let back: models::ResolumeHost = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn resolume_clip_mapping_serde_roundtrip() {
        let m = models::ResolumeClipMapping {
            id: 1,
            resolume_host_id: 1,
            playlist_id: 1,
            layer: 3,
            column: 5,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: models::ResolumeClipMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    // ---- WebSocket messages ----

    #[test]
    fn client_msg_serde_roundtrip() {
        let messages = vec![
            ws::ClientMsg::Play { playlist_id: 1 },
            ws::ClientMsg::Pause { playlist_id: 2 },
            ws::ClientMsg::Skip { playlist_id: 3 },
            ws::ClientMsg::Previous { playlist_id: 4 },
            ws::ClientMsg::SetMode {
                playlist_id: 1,
                mode: playback::PlaybackMode::Loop,
            },
            ws::ClientMsg::SyncPlaylist { playlist_id: 1 },
            ws::ClientMsg::Ping,
        ];
        for msg in messages {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ws::ClientMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back);
        }
    }

    #[test]
    fn server_msg_serde_roundtrip() {
        let messages: Vec<ws::ServerMsg> = vec![
            ws::ServerMsg::NowPlaying {
                playlist_id: 1,
                video_id: 42,
                song: "Song".into(),
                artist: "Artist".into(),
                position_ms: 5000,
                duration_ms: 200000,
            },
            ws::ServerMsg::PlaybackStateChanged {
                playlist_id: 1,
                state: playback::PlaybackState::Playing,
                mode: playback::PlaybackMode::Continuous,
            },
            ws::ServerMsg::QueueUpdate {
                playlist_id: 1,
                video_count: 10,
                cached_count: 5,
            },
            ws::ServerMsg::DownloadProgress {
                playlist_id: 1,
                youtube_id: "abc123".into(),
                title: "Video Title".into(),
                progress_pct: 45.5,
                stage: "downloading".into(),
            },
            ws::ServerMsg::ObsStatus {
                connected: true,
                active_scene: Some("Main".into()),
            },
            ws::ServerMsg::ObsStatus {
                connected: false,
                active_scene: None,
            },
            ws::ServerMsg::ResolumeStatus {
                host_id: 1,
                connected: true,
            },
            ws::ServerMsg::ToolsStatus {
                ytdlp_available: true,
                ffmpeg_available: true,
                ytdlp_version: Some("2025.01.15".into()),
            },
            ws::ServerMsg::ToolsStatus {
                ytdlp_available: false,
                ffmpeg_available: false,
                ytdlp_version: None,
            },
            ws::ServerMsg::Error {
                message: "something went wrong".into(),
            },
            ws::ServerMsg::Pong,
        ];
        for msg in messages {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ws::ServerMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back);
        }
    }

    #[test]
    fn client_msg_json_has_tag_format() {
        let msg = ws::ClientMsg::Play { playlist_id: 1 };
        let json: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "Play");
        assert_eq!(json["data"]["playlist_id"], 1);
    }

    #[test]
    fn server_msg_pong_has_tag_format() {
        let msg = ws::ServerMsg::Pong;
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("Pong"));
    }

    // ---- Config constants ----

    #[test]
    fn config_defaults_are_sensible() {
        assert_eq!(config::DEFAULT_API_PORT, 8920);
        assert_eq!(config::DEFAULT_MAX_RESOLUTION, 1440);
        // config::VERSION comes from env!("CARGO_PKG_VERSION") so it is
        // guaranteed non-empty at compile time — no runtime check needed.
        assert!(config::DEFAULT_OBS_WEBSOCKET_URL.starts_with("ws://"));
    }
}
