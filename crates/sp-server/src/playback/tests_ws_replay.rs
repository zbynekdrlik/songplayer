//! #225: what a dashboard that connects mid-song is told FIRST — the WS
//! on-connect replay (`api::websocket::on_connect_replay`).
//!
//! A playing playlist's replay carries its song (the engine's last
//! `NowPlaying`: song, artist, position, duration — what makes the Player's
//! now-playing content) AHEAD of its state, so the Player never shows
//! "nič nehrá" for a song it was not told about yet. A playlist that plays
//! nothing gets an explicit `Idle` state and no `NowPlaying`, so the Player
//! can tell "nothing plays" from "not known yet".
//!
//! The mode told is the one the engine plays (review round 1): the one it
//! last told the dashboards, else the playlist row's, which its pipeline
//! starts in (#225 unit 2: the row is the mode's one persisted truth). A
//! mode change is told to the dashboards at once, and so to the next one
//! that connects.

use std::path::PathBuf;

use sp_core::playback::{PlaybackMode, PlaybackState, TransportState};
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

use super::pipeline::PipelineEvent;
use super::state::{PlayEvent, PlayState};
use super::{PlaybackEngine, PlaybackEngineConfig};

// Playlist and video ids no other test uses: what the engine tells the
// dashboard may live process-wide, so these tests read back only their own.
const PLAYING: i64 = 22_501;
const IDLE: i64 = 22_502;
const MODE_CHANGED: i64 = 22_503;
const DB_DOWN: i64 = 22_504;
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

/// The engine under test, the channel it tells the dashboard on, and the
/// Resolume channel's receiver (kept alive by the test).
type Rig = (
    PlaybackEngine,
    broadcast::Receiver<ServerMsg>,
    mpsc::Receiver<crate::resolume::ResolumeCommand>,
);

/// An engine on the API state's DB and NDI-health registry.
fn engine_on(state: &crate::AppState) -> Rig {
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, resolume_rx) = mpsc::channel(16);
    let (ws_tx, ws_rx) = broadcast::channel::<ServerMsg>(64);
    let engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool: state.pool.clone(),
        cache_dir: PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: state.ndi_health_registry.clone(),
    });
    (engine, ws_rx, resolume_rx)
}

#[tokio::test]
async fn a_new_dashboard_is_told_the_playing_song_before_its_state_and_an_idle_playlist_is_idle() {
    let state = crate::api::routes::tests::test_state().await;
    // PLAYING's row says `single`, but the test drives its engine straight to
    // Loop below (the API would write the row first). IDLE's row says `loop`:
    // its pipeline starts in it (#225 unit 2).
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, playback_mode) \
         VALUES (?, 'P225 playing', 'url-225a', 'SP-225a', 'single'), \
                (?, 'P225 idle', 'url-225b', 'SP-225b', 'loop')",
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

    let (mut engine, _ws_rx, _resolume_rx) = engine_on(&state);
    engine.ensure_pipeline(PLAYING, "SP-225a");
    engine.ensure_pipeline_for_playlist(IDLE).await;

    // PLAYING plays SONG on program in Loop mode, as a dashboard already
    // connected saw it: the mode change, its state, the song's start
    // (`Started` → NowPlaying at 0:00), then a position tick at 0:42. IDLE
    // never plays.
    if let Some(pp) = engine.pipelines.get_mut(&PLAYING) {
        pp.current_video_id = Some(SONG);
    }
    engine.set_state_for_test(PLAYING, PlayState::Playing { video_id: SONG });
    engine.set_scene_active_for_test(PLAYING, true);
    engine
        .handle_command(PLAYING, PlayEvent::SetMode(PlaybackMode::Loop))
        .await;
    engine.broadcast_state(PLAYING);
    engine
        .handle_pipeline_event(
            PLAYING,
            PipelineEvent::Started {
                duration_ms: 180_000,
                position_ms: 0,
            },
        )
        .await;
    // Review round 4: a dashboard connecting right after the song's start is
    // told it at 0:00 (the `Started` NowPlaying goes through the record too).
    let at_start = crate::api::websocket::on_connect_replay(&state).await;
    assert_eq!(
        replay_of(at_start, &[PLAYING]).first(),
        Some(&ServerMsg::NowPlaying {
            playlist_id: PLAYING,
            video_id: SONG,
            song: "Test Song".into(),
            artist: "Test Artist".into(),
            position_ms: 0,
            duration_ms: 180_000,
        }),
        "the song's start is told to a new dashboard"
    );
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
            // … then its state: on program, the raw transport, the mode the
            // engine plays (Loop), not the DB's `single`.
            ServerMsg::PlaybackStateChanged {
                playlist_id: PLAYING,
                state: PlaybackState::Playing,
                mode: PlaybackMode::Loop,
                transport: TransportState::Playing,
            },
            // A playlist that plays nothing: an explicit Idle, no NowPlaying,
            // in its row's `loop`, the mode its pipeline started in.
            ServerMsg::PlaybackStateChanged {
                playlist_id: IDLE,
                state: PlaybackState::Idle,
                mode: PlaybackMode::Loop,
                transport: TransportState::Idle,
            },
        ],
        "a new dashboard must be told the playing song before its state, and that the idle \
         playlist is idle, each in the mode the engine plays"
    );
}

#[tokio::test]
async fn a_mode_change_is_told_to_the_dashboards_at_once_and_to_the_next_one() {
    let state = crate::api::routes::tests::test_state().await;
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, playback_mode) \
         VALUES (?, 'P225 mode', 'url-225c', 'SP-225c', 'single')",
    )
    .bind(MODE_CHANGED)
    .execute(&state.pool)
    .await
    .unwrap();
    let (mut engine, mut ws_rx, _resolume_rx) = engine_on(&state);
    engine.ensure_pipeline(MODE_CHANGED, "SP-225c");

    // The operator picks "Opakovať" (Loop) on an idle playlist.
    engine
        .handle_command(MODE_CHANGED, PlayEvent::SetMode(PlaybackMode::Loop))
        .await;

    let told = ServerMsg::PlaybackStateChanged {
        playlist_id: MODE_CHANGED,
        state: PlaybackState::Idle,
        mode: PlaybackMode::Loop,
        transport: TransportState::Idle,
    };
    assert_eq!(
        ws_rx.try_recv().ok(),
        Some(told.clone()),
        "the connected dashboards are told the new mode at once"
    );
    let replay = crate::api::websocket::on_connect_replay(&state).await;
    assert_eq!(
        replay_of(replay, &[MODE_CHANGED]),
        vec![told],
        "and so is the next dashboard that connects"
    );
}

/// Review round 2: a failed DB read (no playlist list) still tells a new
/// dashboard every playlist the engine has told the open ones about.
#[tokio::test]
async fn a_failed_playlist_read_still_replays_what_the_engine_told() {
    let state = crate::api::routes::tests::test_state().await;
    let (mut engine, _ws_rx, _resolume_rx) = engine_on(&state);
    engine.ensure_pipeline(DB_DOWN, "SP-225d");
    engine.set_state_for_test(DB_DOWN, PlayState::Playing { video_id: 7 });
    engine.broadcast_state(DB_DOWN);

    // The playlist read fails from here on.
    state.pool.close().await;

    let replay = crate::api::websocket::on_connect_replay(&state).await;
    assert_eq!(
        replay_of(replay, &[DB_DOWN]),
        vec![ServerMsg::PlaybackStateChanged {
            playlist_id: DB_DOWN,
            state: PlaybackState::WaitingForScene,
            mode: PlaybackMode::Continuous,
            transport: TransportState::Playing,
        }]
    );
}
