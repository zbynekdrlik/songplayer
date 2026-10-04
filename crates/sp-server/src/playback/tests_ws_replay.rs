//! #225: what a dashboard that connects mid-song is told FIRST — the WS
//! on-connect replay (`api::websocket::on_connect_replay`).
//!
//! A playing playlist's replay carries its song (the engine's last
//! `NowPlaying`: song, artist, position, duration — what makes the Player's
//! now-playing content) AHEAD of its state, so the Player never shows
//! "nič nehrá" for a song it was not told about yet. A playlist that plays
//! nothing gets an explicit `Idle` state and no `NowPlaying`, so the Player
//! can tell "nothing plays" from "not known yet".

use std::path::PathBuf;

use sp_core::playback::{PlaybackMode, PlaybackState, TransportState};
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

use super::pipeline::PipelineEvent;
use super::state::PlayState;
use super::{PlaybackEngine, PlaybackEngineConfig};

// Playlist and video ids no other test uses: what the engine tells the
// dashboard may live process-wide, so this test reads back only its own.
const PLAYING: i64 = 22_501;
const IDLE: i64 = 22_502;
const SONG: i64 = 22_542;

/// The replay's messages about `ids`, in the order a new client gets them.
fn replay_of(replay: Vec<ServerMsg>, ids: &[i64]) -> Vec<ServerMsg> {
    replay
        .into_iter()
        .filter(|m| match m {
            ServerMsg::NowPlaying { playlist_id, .. }
            | ServerMsg::PlaybackStateChanged { playlist_id, .. } => ids.contains(playlist_id),
            _ => false,
        })
        .collect()
}

#[tokio::test]
async fn a_new_dashboard_is_told_the_playing_song_before_its_state_and_an_idle_playlist_is_idle() {
    let state = crate::api::routes::tests::test_state().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, playback_mode) \
         VALUES (?, 'P225 playing', 'url-225a', 'SP-225a', 'loop'), \
                (?, 'P225 idle', 'url-225b', 'SP-225b', 'continuous')",
    )
    .bind(PLAYING)
    .bind(IDLE)
    .execute(&state.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist) \
         VALUES (?, ?, 'yt225', 'Test Song', 'Test Artist')",
    )
    .bind(SONG)
    .bind(PLAYING)
    .execute(&state.pool)
    .await
    .unwrap();

    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _resolume_rx) = mpsc::channel(16);
    let (ws_tx, _ws_rx) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool: state.pool.clone(),
        cache_dir: PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: state.ndi_health_registry.clone(),
    });
    engine.ensure_pipeline(PLAYING, "SP-225a");
    engine.ensure_pipeline(IDLE, "SP-225b");

    // PLAYING plays SONG on program, as a dashboard already connected saw it:
    // its state, the song's start (`Started` → NowPlaying at 0:00), then a
    // position tick at 0:42. IDLE never plays.
    if let Some(pp) = engine.pipelines.get_mut(&PLAYING) {
        pp.current_video_id = Some(SONG);
    }
    engine.set_state_for_test(PLAYING, PlayState::Playing { video_id: SONG });
    engine.set_scene_active_for_test(PLAYING, true);
    engine.broadcast_state(PLAYING);
    engine
        .handle_pipeline_event(
            PLAYING,
            PipelineEvent::Started {
                duration_ms: 180_000,
            },
        )
        .await;
    // Past the 500 ms position throttle, so the tick goes out.
    if let Some(pp) = engine.pipelines.get_mut(&PLAYING) {
        pp.last_now_playing_broadcast = None;
    }
    engine.maybe_broadcast_position_update(PLAYING, 42_000, 180_000);

    // A dashboard connects now.
    let replay = crate::api::websocket::on_connect_replay(&state).await;
    assert_eq!(
        replay_of(replay, &[PLAYING, IDLE]),
        vec![
            // The song first, at the position the engine last told the
            // dashboard — so the state never lands without its song …
            ServerMsg::NowPlaying {
                playlist_id: PLAYING,
                video_id: SONG,
                song: "Test Song".into(),
                artist: "Test Artist".into(),
                position_ms: 42_000,
                duration_ms: 180_000,
            },
            // … then its state: on program, the raw transport, the DB's mode.
            ServerMsg::PlaybackStateChanged {
                playlist_id: PLAYING,
                state: PlaybackState::Playing,
                mode: PlaybackMode::Loop,
                transport: TransportState::Playing,
            },
            // A playlist that plays nothing: an explicit Idle, no NowPlaying.
            ServerMsg::PlaybackStateChanged {
                playlist_id: IDLE,
                state: PlaybackState::Idle,
                mode: PlaybackMode::Continuous,
                transport: TransportState::Idle,
            },
        ],
        "a new dashboard must be told the playing song before its state, and that the idle \
         playlist is idle"
    );
}
