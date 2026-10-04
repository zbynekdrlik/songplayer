//! #225: the on-connect replay's rules (`DashboardReplay`), on a private
//! instance; the engine glue (`send_dashboard`, `remove_pipeline`) on the
//! process-global one, with playlist ids no other test uses.

use std::path::PathBuf;

use sp_core::playback::{PlaybackMode, PlaybackState, TransportState};
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

use super::super::state::PlayState;
use super::super::{PlaybackEngine, PlaybackEngineConfig};
use super::{DashboardReplay, global};

fn state(
    playlist_id: i64,
    state: PlaybackState,
    mode: PlaybackMode,
    transport: TransportState,
) -> ServerMsg {
    ServerMsg::PlaybackStateChanged {
        playlist_id,
        state,
        mode,
        transport,
    }
}

fn song(playlist_id: i64, video_id: i64, position_ms: u64) -> ServerMsg {
    ServerMsg::NowPlaying {
        playlist_id,
        video_id,
        song: format!("Song {video_id}"),
        artist: "Artist".into(),
        position_ms,
        duration_ms: 180_000,
    }
}

fn idle(playlist_id: i64, mode: PlaybackMode) -> ServerMsg {
    state(playlist_id, PlaybackState::Idle, mode, TransportState::Idle)
}

#[test]
fn a_playing_playlist_replays_its_last_song_then_its_state_and_an_idle_one_only_idle() {
    let r = DashboardReplay::default();
    r.record(&state(
        1,
        PlaybackState::Playing,
        PlaybackMode::Continuous,
        TransportState::Playing,
    ));
    r.record(&song(1, 10, 0));
    // A position tick replaces the song's start: the LAST NowPlaying is told.
    r.record(&song(1, 10, 42_000));

    assert_eq!(
        r.replay(&[(1, PlaybackMode::Loop), (2, PlaybackMode::Single)]),
        vec![
            song(1, 10, 42_000),
            // The DB's mode, not the recorded one (a mode change broadcasts
            // no state).
            state(
                1,
                PlaybackState::Playing,
                PlaybackMode::Loop,
                TransportState::Playing
            ),
            // Never recorded: an explicit Idle, no song.
            idle(2, PlaybackMode::Single),
        ]
    );
}

#[test]
fn a_playlist_that_went_idle_replays_idle_and_not_its_old_song() {
    let r = DashboardReplay::default();
    r.record(&state(
        3,
        PlaybackState::Playing,
        PlaybackMode::Loop,
        TransportState::Playing,
    ));
    r.record(&song(3, 30, 5_000));
    r.record(&idle(3, PlaybackMode::Loop));

    assert_eq!(
        r.replay(&[(3, PlaybackMode::Single)]),
        vec![idle(3, PlaybackMode::Single)]
    );
}

#[test]
fn an_off_program_or_paused_playlist_replays_its_song_and_its_raw_transport() {
    let r = DashboardReplay::default();
    // Decoding off program (#201): WaitingForScene with transport Playing.
    r.record(&song(4, 40, 1_000));
    r.record(&state(
        4,
        PlaybackState::WaitingForScene,
        PlaybackMode::Continuous,
        TransportState::Playing,
    ));
    // Paused, its song still loaded.
    r.record(&state(
        5,
        PlaybackState::WaitingForScene,
        PlaybackMode::Continuous,
        TransportState::Paused,
    ));
    r.record(&song(5, 50, 9_000));

    assert_eq!(
        r.replay(&[(4, PlaybackMode::Continuous), (5, PlaybackMode::Continuous)]),
        vec![
            song(4, 40, 1_000),
            state(
                4,
                PlaybackState::WaitingForScene,
                PlaybackMode::Continuous,
                TransportState::Playing
            ),
            song(5, 50, 9_000),
            state(
                5,
                PlaybackState::WaitingForScene,
                PlaybackMode::Continuous,
                TransportState::Paused
            ),
        ]
    );
}

#[test]
fn a_state_without_a_song_replays_only_the_state() {
    let r = DashboardReplay::default();
    // Videos available, never played: waiting for its scene, no song yet.
    r.record(&state(
        6,
        PlaybackState::WaitingForScene,
        PlaybackMode::Continuous,
        TransportState::Paused,
    ));

    assert_eq!(
        r.replay(&[(6, PlaybackMode::Continuous)]),
        vec![state(
            6,
            PlaybackState::WaitingForScene,
            PlaybackMode::Continuous,
            TransportState::Paused
        )]
    );
}

#[test]
fn a_forgotten_playlist_replays_idle_while_listed_and_nothing_once_unlisted() {
    let r = DashboardReplay::default();
    r.record(&state(
        7,
        PlaybackState::Playing,
        PlaybackMode::Continuous,
        TransportState::Playing,
    ));
    r.record(&song(7, 70, 3_000));
    r.forget(7);

    assert_eq!(
        r.replay(&[(7, PlaybackMode::Continuous)]),
        vec![idle(7, PlaybackMode::Continuous)]
    );
    assert_eq!(r.replay(&[]), Vec::<ServerMsg>::new());
}

#[test]
fn a_recorded_playlist_the_list_does_not_name_follows_by_id_with_its_recorded_mode() {
    let r = DashboardReplay::default();
    r.record(&song(9, 90, 2_000));
    r.record(&state(
        9,
        PlaybackState::Playing,
        PlaybackMode::Loop,
        TransportState::Playing,
    ));
    r.record(&idle(8, PlaybackMode::Single));
    // A song but no state yet: unknown state → Idle, the default mode.
    r.record(&song(11, 110, 0));
    // The listed playlist is recorded too, so it must not come twice.
    r.record(&idle(1, PlaybackMode::Loop));

    assert_eq!(
        r.replay(&[(1, PlaybackMode::Continuous)]),
        vec![
            idle(1, PlaybackMode::Continuous),
            idle(8, PlaybackMode::Single),
            song(9, 90, 2_000),
            state(
                9,
                PlaybackState::Playing,
                PlaybackMode::Loop,
                TransportState::Playing
            ),
            idle(11, PlaybackMode::Continuous),
        ]
    );
}

#[test]
fn messages_that_are_not_a_playlists_state_or_song_are_not_recorded() {
    let r = DashboardReplay::default();
    r.record(&ServerMsg::Pong);
    r.record(&ServerMsg::LyricsUpdate {
        playlist_id: 12,
        line_en: Some("line".into()),
        line_sk: None,
        prev_line_en: None,
        next_line_en: None,
        active_word_index: None,
        word_count: None,
    });

    assert_eq!(r.replay(&[]), Vec::<ServerMsg>::new());
}

#[test]
fn global_is_one_registry() {
    assert!(std::ptr::eq(global(), global()));
}

// The engine glue on the process-global registry: ids no other test uses.
const RECORDED: i64 = 22_511;
const REMOVED: i64 = 22_512;

/// The global replay's messages about `playlist_id` alone (other tests
/// record into it too).
fn global_replay_of(playlist_id: i64) -> Vec<ServerMsg> {
    global()
        .replay(&[(playlist_id, PlaybackMode::Continuous)])
        .into_iter()
        .filter(|m| match m {
            ServerMsg::NowPlaying { playlist_id: p, .. }
            | ServerMsg::PlaybackStateChanged { playlist_id: p, .. } => *p == playlist_id,
            _ => false,
        })
        .collect()
}

async fn engine() -> (PlaybackEngine, broadcast::Receiver<ServerMsg>) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, ws_rx) = broadcast::channel::<ServerMsg>(16);
    let engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
    (engine, ws_rx)
}

#[tokio::test]
async fn the_engine_records_what_it_tells_the_dashboard_and_still_broadcasts_it() {
    let (mut engine, mut ws_rx) = engine().await;
    engine.ensure_pipeline(RECORDED, "SP-225-recorded");
    engine.set_state_for_test(RECORDED, PlayState::Playing { video_id: 1 });

    engine.broadcast_state(RECORDED);

    let told = state(
        RECORDED,
        PlaybackState::WaitingForScene,
        PlaybackMode::Continuous,
        TransportState::Playing,
    );
    assert_eq!(
        ws_rx.try_recv().unwrap(),
        told,
        "broadcast to the dashboard"
    );
    assert_eq!(
        global_replay_of(RECORDED),
        vec![told],
        "and recorded for the next dashboard that connects"
    );
}

#[tokio::test]
async fn a_removed_pipeline_is_forgotten() {
    let (mut engine, _ws_rx) = engine().await;
    engine.ensure_pipeline(REMOVED, "SP-225-removed");
    engine.set_state_for_test(REMOVED, PlayState::Playing { video_id: 2 });
    engine.set_scene_active_for_test(REMOVED, true);
    engine.broadcast_state(REMOVED);

    engine.remove_pipeline(REMOVED);

    assert_eq!(
        global_replay_of(REMOVED),
        vec![idle(REMOVED, PlaybackMode::Continuous)]
    );
}
