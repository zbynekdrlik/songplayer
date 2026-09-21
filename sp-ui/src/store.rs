//! Reactive store holding all dashboard state as fine-grained signals.

use std::collections::HashMap;

use leptos::prelude::*;
use sp_core::models::*;
use sp_core::playback::*;
use sp_core::ws::ServerMsg;

use crate::api::{HostHealth, NdiOutputHealth};

/// Lyrics pipeline queue state reflected from server WebSocket updates.
#[derive(Debug, Clone, PartialEq)]
pub struct LyricsQueueInfo {
    pub bucket0: i64,
    pub bucket1: i64,
    pub bucket2: i64,
    pub pipeline_version: u32,
    pub processing: Option<LyricsProcessingState>,
}

/// Processing state for a single song currently in the lyrics pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct LyricsProcessingState {
    pub video_id: i64,
    pub youtube_id: String,
    pub song: String,
    pub artist: String,
    pub stage: String,
    pub provider: Option<String>,
    pub started_at_unix_ms: i64,
}

/// A single row from the `/api/v1/lyrics/songs` endpoint.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct LyricsSongEntry {
    pub video_id: i64,
    pub youtube_id: String,
    pub title: Option<String>,
    pub song: Option<String>,
    pub artist: Option<String>,
    pub source: Option<String>,
    pub pipeline_version: i64,
    pub quality_score: Option<f64>,
    pub has_lyrics: bool,
    pub is_stale: bool,
    pub manual_priority: bool,
    /// `videos.lyrics_reference` (#142) — Claude's verified reference
    /// lyrics; the row renders a ★ badge + „Nesedí" feedback button.
    #[serde(default)]
    pub lyrics_reference: bool,
    /// `videos.lyrics_translation_gender` (#152) — per-song SK translation
    /// gender override: `None` = auto (masculine default), `"m"`, or `"f"`.
    /// The row renders a ♂/♀ toggle bound to this value.
    #[serde(default)]
    pub translation_gender: Option<String>,
}

/// One dub-requested video for the Dabing section (#180). Mirrors the server's
/// `models_dabing::DubRow` JSON; `#[serde(default)]` throughout so a partial or
/// newer payload still deserialises.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
pub struct DubRow {
    pub video_id: i64,
    #[serde(default)]
    pub playlist_id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub dub_status: String,
    #[serde(default)]
    pub dub_error: Option<String>,
    #[serde(default)]
    pub dub_mix_ratio: f64,
    #[serde(default)]
    pub dub_file_path: Option<String>,
    #[serde(default)]
    pub stem_status: Option<String>,
    #[serde(default)]
    pub lyrics_present: bool,
    /// Resolved chain-state wire string (`queued`/`stems`/`transcript`/
    /// `translation`/`synth`/`ready`/`failed`) — the server derives it.
    #[serde(default)]
    pub chain_state: String,
    /// The pinned dub voice (#184 round C), from the repurposed
    /// `dub_voice_ref_path` column. `None` until the worker resolves one.
    #[serde(default)]
    pub dub_voice: Option<String>,
}

/// Outcome of the most recent POST /api/v1/lyrics/reprocess (any flavor).
/// Used to surface `blocked_by_asr_gap > 0` to the operator (#98).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReprocessOutcome {
    pub queued: i64,
    pub blocked_by_asr_gap: i64,
}

/// Information about what is currently playing on a playlist.
#[derive(Debug, Clone)]
pub struct NowPlayingInfo {
    pub video_id: i64,
    pub song: String,
    pub artist: String,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub state: PlaybackState,
    /// #201: the pipeline's own transport state, independent of `state`'s
    /// on/off-program folding. The Player's play/pause label reads this.
    pub transport: TransportState,
    pub mode: PlaybackMode,
    pub line_en: Option<String>,
    pub line_sk: Option<String>,
    pub prev_line_en: Option<String>,
    pub next_line_en: Option<String>,
    pub active_word_index: Option<usize>,
    pub word_count: Option<usize>,
}

impl NowPlayingInfo {
    /// True when this entry carries real now-playing content. The
    /// `PlaybackStateChanged`-only shape (a live state with no preceding
    /// `NowPlaying`) inserts a zero entry — empty song, zero duration — which
    /// the card must render as idle, not as a "0:00 / 0:00" now-playing block
    /// (#170).
    pub fn has_now_playing_content(&self) -> bool {
        !self.song.is_empty() || self.duration_ms > 0
    }
}

/// #194 ROUND 3b: the external-tool availability last reported by
/// `ServerMsg::ToolsStatus`. `None` until the first `ToolsStatus` arrives; the
/// `HealthBar` shows a grey `Nástroje: —` until then.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolsInfo {
    pub ytdlp_available: bool,
    pub ffmpeg_available: bool,
    pub ytdlp_version: Option<String>,
    pub js_runtime_ok: bool,
    pub deno_version: Option<String>,
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
    pub lyrics_queue: RwSignal<Option<LyricsQueueInfo>>,
    pub lyrics_songs: RwSignal<Vec<LyricsSongEntry>>,
    pub last_reprocess: RwSignal<Option<ReprocessOutcome>>,
    /// #180: dub-requested videos for the Dabing section. #184: refreshed by a
    /// 2 s poll owned by `App` (not the Dabing page), so `store.dabing` exists on
    /// every page and the shared Player's mixer slot can pick the dub mixer for a
    /// playing dub video on the Dashboard / Live too, not only after visiting
    /// /dabing.
    pub dabing: RwSignal<Vec<DubRow>>,
    /// #184: the seeded Dabing playlist id, resolved from the `/api/v1/dabing`
    /// payload by the App-level poll. The Dabing page reads it to mount the
    /// shared Player for that output. `None` until the first payload arrives.
    pub dabing_playlist_id: RwSignal<Option<i64>>,
    /// Per-output NDI genlock health (#150), refreshed ~1 Hz by the
    /// dashboard's `GlobalLockBadge` poll loop and read by every per-card
    /// `LockBadge`.
    pub ndi_health: RwSignal<Vec<NdiOutputHealth>>,
    /// #194 ROUND 3b: Resolume push-chain health, refreshed by the shared
    /// `HealthBar`'s 5 s poll (was `resolume_health.rs`'s own loop).
    pub resolume_health: RwSignal<Vec<HostHealth>>,
    /// #194 ROUND 3b: external tool availability from `ServerMsg::ToolsStatus`
    /// (previously discarded). Read by the `HealthBar` tools segment.
    pub tools: RwSignal<Option<ToolsInfo>>,
    /// #165: which playlist the single dashboard work area shows. `None` until
    /// the first playlist load resolves it (persisted → playing → first).
    pub selected_playlist: RwSignal<Option<i64>>,
    /// #165: `true` once the selection is user-driven (a click / `<select>` /
    /// "Prejsť") or restored from the URL/localStorage — the auto-follow Effect
    /// then stops tracking the playing playlist so a reload keeps the operator's
    /// choice instead of snapping back to whatever is playing.
    pub selection_pinned: RwSignal<bool>,
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
            lyrics_queue: RwSignal::new(None),
            lyrics_songs: RwSignal::new(vec![]),
            last_reprocess: RwSignal::new(None),
            dabing: RwSignal::new(vec![]),
            dabing_playlist_id: RwSignal::new(None),
            ndi_health: RwSignal::new(vec![]),
            resolume_health: RwSignal::new(vec![]),
            tools: RwSignal::new(None),
            selected_playlist: RwSignal::new(None),
            selection_pinned: RwSignal::new(false),
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
                        transport: TransportState::default(),
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
                transport,
            } => {
                self.now_playing.update(|map| {
                    if let Some(entry) = map.get_mut(&playlist_id) {
                        entry.state = state;
                        entry.transport = transport;
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
                                transport,
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
            ServerMsg::LyricsQueueUpdate {
                bucket0_count,
                bucket1_count,
                bucket2_count,
                pipeline_version,
                processing,
            } => {
                self.lyrics_queue.set(Some(LyricsQueueInfo {
                    bucket0: bucket0_count,
                    bucket1: bucket1_count,
                    bucket2: bucket2_count,
                    pipeline_version,
                    processing: processing.map(|p| LyricsProcessingState {
                        video_id: p.video_id,
                        youtube_id: p.youtube_id,
                        song: p.song,
                        artist: p.artist,
                        stage: p.stage,
                        provider: p.provider,
                        started_at_unix_ms: p.started_at_unix_ms,
                    }),
                }));
            }
            ServerMsg::LyricsProcessingStage {
                video_id,
                youtube_id,
                stage,
                provider,
            } => {
                self.lyrics_queue.update(|q| {
                    if let Some(info) = q {
                        info.processing = Some(LyricsProcessingState {
                            video_id,
                            youtube_id,
                            song: String::new(),
                            artist: String::new(),
                            stage,
                            provider,
                            started_at_unix_ms: 0,
                        });
                    }
                });
            }
            ServerMsg::LyricsCompleted {
                video_id,
                source,
                quality_score,
                ..
            } => {
                self.lyrics_songs.update(|list| {
                    if let Some(entry) = list.iter_mut().find(|e| e.video_id == video_id) {
                        entry.source = Some(source);
                        entry.quality_score = Some(quality_score as f64);
                        entry.has_lyrics = true;
                        entry.is_stale = false;
                        entry.manual_priority = false;
                    }
                });
            }
            ServerMsg::ToolsStatus {
                ytdlp_available,
                ffmpeg_available,
                ytdlp_version,
                js_runtime_ok,
                deno_version,
            } => {
                self.tools.set(Some(ToolsInfo {
                    ytdlp_available,
                    ffmpeg_available,
                    ytdlp_version,
                    js_runtime_ok,
                    deno_version,
                }));
            }
            ServerMsg::Pong
            | ServerMsg::QueueUpdate { .. }
            | ServerMsg::ResolumeStatus { .. }
            | ServerMsg::KaraokeStateChanged { .. } => {
                // Informational; the karaoke control component owns its own
                // mode/gain state via GET/POST, so no store update needed.
            }
        }
    }
}

/// #194 ROUND 3b: ONE cancellable polling helper.
///
/// Every hand-rolled poll loop (`ndi_health`, `resolume_health`, `dabing`)
/// re-implemented the identical `cancelled` / `try_get_untracked` / `try_set`
/// dance (`sp-ui-frontend.md`: a `spawn_local` task outlives its reactive owner,
/// so a wake after navigation must NOT `get()` a disposed signal). This is that
/// dance in one place.
///
/// `cancelled` is the caller's page-owned flag (flip it in `on_cleanup`).
/// `target` receives each successful fetch via `try_set`; a disposed target
/// (component unmounted mid-fetch) stops the loop instead of panicking.
pub fn poll_into<T>(
    endpoint: &'static str,
    interval_ms: u32,
    cancelled: RwSignal<bool>,
    target: RwSignal<T>,
) where
    // `RwSignal<T>` uses the default `SyncStorage`, so `T: Send + Sync` is
    // required for the signature itself to be well-formed (both call sites —
    // `Vec<NdiOutputHealth>` / `Vec<HostHealth>` — satisfy it).
    T: serde::de::DeserializeOwned + Send + Sync + 'static,
{
    leptos::task::spawn_local(async move {
        loop {
            // `cancelled` is page-owned: navigating away disposes it while the
            // task is parked in the timer, so the next wake uses
            // `try_get_untracked` (None on a disposed signal) and stops.
            if cancelled.try_get_untracked() != Some(false) {
                break;
            }
            if let Ok(data) = crate::api::get::<T>(endpoint).await
                && target.try_set(data).is_some()
            {
                break; // target disposed — stop rather than panic on a later read
            }
            gloo_timers::future::TimeoutFuture::new(interval_ms).await;
        }
    });
}

/// Poll variant for endpoints whose JSON needs post-processing before it lands
/// in one or more signals (e.g. the Dabing payload `{playlist_id, videos:[…]}`).
/// `apply` receives each successful `serde_json::Value` and returns `true` to
/// STOP the loop (a disposed target signal), mirroring `try_set`'s contract.
pub fn poll_value<F>(endpoint: &'static str, interval_ms: u32, cancelled: RwSignal<bool>, apply: F)
where
    F: Fn(serde_json::Value) -> bool + 'static,
{
    leptos::task::spawn_local(async move {
        loop {
            if cancelled.try_get_untracked() != Some(false) {
                break;
            }
            if let Ok(v) = crate::api::get::<serde_json::Value>(endpoint).await
                && apply(v)
            {
                break;
            }
            gloo_timers::future::TimeoutFuture::new(interval_ms).await;
        }
    });
}
