//! Reactive store holding all dashboard state as fine-grained signals.

use std::collections::HashMap;

use leptos::prelude::*;
use sp_core::models::*;
use sp_core::playback::*;
use sp_core::ws::ServerMsg;

/// Information about what is currently playing on a playlist.
#[derive(Debug, Clone)]
pub struct NowPlayingInfo {
    pub video_id: i64,
    pub song: String,
    pub artist: String,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub state: PlaybackState,
    pub mode: PlaybackMode,
    pub line_en: Option<String>,
    pub line_sk: Option<String>,
    pub prev_line_en: Option<String>,
    pub next_line_en: Option<String>,
    pub active_word_index: Option<usize>,
    pub word_count: Option<usize>,
}

/// A single item in the download queue.
#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub playlist_id: i64,
    pub youtube_id: String,
    pub title: String,
    pub progress_pct: f32,
    pub stage: String,
}

/// Central reactive store provided via Leptos context.
#[derive(Debug, Clone, Copy)]
pub struct DashboardStore {
    pub playlists: RwSignal<Vec<Playlist>>,
    pub now_playing: RwSignal<HashMap<i64, NowPlayingInfo>>,
    pub download_queue: RwSignal<Vec<DownloadItem>>,
    pub obs_connected: RwSignal<bool>,
    pub obs_scene: RwSignal<Option<String>>,
    pub ws_connected: RwSignal<bool>,
    pub errors: RwSignal<Vec<String>>,
    pub settings: RwSignal<HashMap<String, String>>,
    pub resolume_hosts: RwSignal<Vec<ResolumeHost>>,
}

impl DashboardStore {
    pub fn new() -> Self {
        Self {
            playlists: RwSignal::new(vec![]),
            now_playing: RwSignal::new(HashMap::new()),
            download_queue: RwSignal::new(vec![]),
            obs_connected: RwSignal::new(false),
            obs_scene: RwSignal::new(None),
            ws_connected: RwSignal::new(false),
            errors: RwSignal::new(vec![]),
            settings: RwSignal::new(HashMap::new()),
            resolume_hosts: RwSignal::new(vec![]),
        }
    }

    /// Dispatch a [`ServerMsg`] to the appropriate signal.
    pub fn dispatch(&self, msg: ServerMsg) {
        match msg {
            ServerMsg::NowPlaying {
                playlist_id,
                video_id,
                song,
                artist,
                position_ms,
                duration_ms,
            } => {
                self.now_playing.update(|map| {
                    let entry = map.entry(playlist_id).or_insert_with(|| NowPlayingInfo {
                        video_id,
                        song: String::new(),
                        artist: String::new(),
                        position_ms: 0,
                        duration_ms: 0,
                        state: PlaybackState::default(),
                        mode: PlaybackMode::default(),
                        line_en: None,
                        line_sk: None,
                        prev_line_en: None,
                        next_line_en: None,
                        active_word_index: None,
                        word_count: None,
                    });
                    entry.video_id = video_id;
                    entry.song = song;
                    entry.artist = artist;
                    entry.position_ms = position_ms;
                    entry.duration_ms = duration_ms;
                });
            }
            ServerMsg::PlaybackStateChanged {
                playlist_id,
                state,
                mode,
            } => {
                self.now_playing.update(|map| {
                    if let Some(entry) = map.get_mut(&playlist_id) {
                        entry.state = state;
                        entry.mode = mode;
                    } else {
                        map.insert(
                            playlist_id,
                            NowPlayingInfo {
                                video_id: 0,
                                song: String::new(),
                                artist: String::new(),
                                position_ms: 0,
                                duration_ms: 0,
                                state,
                                mode,
                                line_en: None,
                                line_sk: None,
                                prev_line_en: None,
                                next_line_en: None,
                                active_word_index: None,
                                word_count: None,
                            },
                        );
                    }
                });
            }
            ServerMsg::DownloadProgress {
                playlist_id,
                youtube_id,
                title,
                progress_pct,
                stage,
            } => {
                self.download_queue.update(|queue| {
                    if let Some(item) = queue.iter_mut().find(|i| i.youtube_id == youtube_id) {
                        item.progress_pct = progress_pct;
                        item.stage = stage;
                    } else {
                        queue.push(DownloadItem {
                            playlist_id,
                            youtube_id,
                            title,
                            progress_pct,
                            stage,
                        });
                    }
                    // Remove completed downloads.
                    queue.retain(|i| i.progress_pct < 100.0);
                });
            }
            ServerMsg::ObsStatus {
                connected,
                active_scene,
            } => {
                self.obs_connected.set(connected);
                self.obs_scene.set(active_scene);
            }
            ServerMsg::Error { message } => {
                self.errors.update(|errs| {
                    errs.push(message);
                    // Keep only the last 50 errors.
                    if errs.len() > 50 {
                        errs.drain(0..errs.len() - 50);
                    }
                });
            }
            ServerMsg::LyricsUpdate {
                playlist_id,
                line_en,
                line_sk,
                prev_line_en,
                next_line_en,
                active_word_index,
                word_count,
            } => {
                self.now_playing.update(|map| {
                    if let Some(info) = map.get_mut(&playlist_id) {
                        info.line_en = line_en;
                        info.line_sk = line_sk;
                        info.prev_line_en = prev_line_en;
                        info.next_line_en = next_line_en;
                        info.active_word_index = active_word_index;
                        info.word_count = word_count;
                    }
                });
            }
            ServerMsg::Pong
            | ServerMsg::QueueUpdate { .. }
            | ServerMsg::ResolumeStatus { .. }
            | ServerMsg::ToolsStatus { .. } => {
                // These are informational; no store update needed yet.
            }
        }
    }
}
