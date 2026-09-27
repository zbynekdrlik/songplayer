//! Regression tests for the 2026-04-19 event cross-playlist Resolume
//! bleed bug. Sibling file so `playback/mod.rs` and `playback/tests.rs`
//! each stay under the 1000-line airuleset cap.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use super::pipeline::PipelineEvent;
use super::*;
use crate::resolume::ResolumeCommand;
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

/// When a playlist transitions off-program (scene_active: true → false),
/// `handle_scene_change` MUST send `HideTitle` + `HideSubtitles` to the
/// resolume channel so the now-background playlist doesn't leave its
/// title/subs on the shared Resolume clips. Without this, the on-program
/// playlist's text gets clobbered by whatever the off-program playlist
/// last displayed — the exact bug that made the event unusable.
#[tokio::test]
async fn handle_scene_change_off_sends_hide_title_and_subs() {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    let (obs_tx, _obs_rx) = broadcast::channel(16);
    let (resolume_tx, mut resolume_rx) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });

    engine.ensure_pipeline(7, "SP-fast");
    // Force the pipeline into scene_active = true so the transition
    // downward is what we're measuring.
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.scene_active
            .store(true, std::sync::atomic::Ordering::Release);
    }

    engine.handle_scene_change(7, false).await;

    let mut cmds: Vec<crate::resolume::ResolumeCommand> = Vec::new();
    while let Ok(cmd) = resolume_rx.try_recv() {
        cmds.push(cmd);
    }

    let has_hide_title = cmds
        .iter()
        .any(|c| matches!(c, crate::resolume::ResolumeCommand::HideTitle));
    let has_hide_subs = cmds
        .iter()
        .any(|c| matches!(c, crate::resolume::ResolumeCommand::HideSubtitles));
    assert!(
        has_hide_title,
        "handle_scene_change(off) MUST send HideTitle to clear the shared \
         #sp-title clip. Got: {cmds:?}"
    );
    assert!(
        has_hide_subs,
        "handle_scene_change(off) MUST send HideSubtitles too. Got: {cmds:?}"
    );
}

/// Negative: handle_scene_change(off) on a pipeline that was ALREADY
/// off-program must NOT send redundant Hide commands — only the
/// true→false transition should fire the gate.
#[tokio::test]
async fn handle_scene_change_off_noop_when_already_off_program() {
    let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
    let (obs_tx, _obs_rx) = broadcast::channel(16);
    let (resolume_tx, mut resolume_rx) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });

    engine.ensure_pipeline(7, "SP-fast");
    // Pipeline is created with scene_active = false (default).
    engine.handle_scene_change(7, false).await;

    let mut cmds: Vec<crate::resolume::ResolumeCommand> = Vec::new();
    while let Ok(cmd) = resolume_rx.try_recv() {
        cmds.push(cmd);
    }
    let had_hide = cmds.iter().any(|c| {
        matches!(
            c,
            crate::resolume::ResolumeCommand::HideTitle
                | crate::resolume::ResolumeCommand::HideSubtitles
        )
    });
    assert!(
        !had_hide,
        "handle_scene_change(off) on an already-off-program pipeline must \
         NOT send Hide commands — only the true→false transition should. \
         Got: {cmds:?}"
    );
}

// -- #217 addendum 3: the wall title is re-synced, per the song's window -----
//
// A recovery and an OBS scene-on send the driver ONE `Resync` naming the title
// that SHOULD be up: the song title of a playing, on-program pipeline that
// has had its `Started` and is inside its title window (1.5 s after the
// start to 3.5 s before the end), else none. The driver owns the wall and
// acts only on a difference (`resolume/driver_title_tests.rs`).

/// Every test song is this long.
const SONG_MS: u64 = 180_000;

/// An engine with one pipeline per `(playlist, video, song)`, each video by
/// "Artist".
async fn test_engine(
    songs: &[(i64, i64, &str)],
) -> (PlaybackEngine, mpsc::Receiver<ResolumeCommand>) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for &(playlist_id, video_id, song) in songs {
        sqlx::query(
            "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
             VALUES (?, 'p', 'u', ?, 1)",
        )
        .bind(playlist_id)
        .bind(format!("SP-{playlist_id}"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized) \
             VALUES (?, ?, ?, ?, 'Artist', 1)",
        )
        .bind(video_id)
        .bind(playlist_id)
        .bind(format!("yt{video_id}"))
        .bind(song)
        .execute(&pool)
        .await
        .unwrap();
    }
    let (obs_tx, _obs_rx) = broadcast::channel(16);
    let (resolume_tx, resolume_rx) = mpsc::channel(64);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(16);
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: std::sync::Arc::new(
            crate::playback::ndi_health::NdiHealthRegistry::new(),
        ),
    });
    for &(playlist_id, _, _) in songs {
        engine.ensure_pipeline(playlist_id, &format!("SP-{playlist_id}"));
    }
    (engine, resolume_rx)
}

/// Playlist `playlist_id` plays `video_id` on program, `position_ms` into its
/// `SONG_MS`; `started` = the video whose `Started` the engine last handled.
fn play(
    engine: &mut PlaybackEngine,
    playlist_id: i64,
    video_id: i64,
    position_ms: u64,
    started: Option<i64>,
) {
    let pp = engine.pipelines.get_mut(&playlist_id).expect("pipeline");
    pp.state = PlayState::Playing { video_id };
    pp.current_video_id = Some(video_id);
    pp.started_video_id = started;
    pp.cached_position_ms = position_ms;
    pp.cached_duration_ms = SONG_MS;
    pp.scene_active.store(true, Ordering::Release);
}

/// Every command waiting on the Resolume channel.
fn sent(rx: &mut mpsc::Receiver<ResolumeCommand>) -> Vec<ResolumeCommand> {
    let mut cmds = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        cmds.push(cmd);
    }
    cmds
}

/// The title of every `Resync` in `cmds` (`None` = a resync naming no title).
fn resyncs(cmds: &[ResolumeCommand]) -> Vec<Option<String>> {
    cmds.iter()
        .filter_map(|cmd| match cmd {
            ResolumeCommand::Resync { title } => Some(title.clone()),
            _ => None,
        })
        .collect()
}

fn shows_title(cmds: &[ResolumeCommand]) -> bool {
    cmds.iter()
        .any(|cmd| matches!(cmd, ResolumeCommand::ShowTitle { .. }))
}

/// The commands one recovery sends for playlist 7 playing video 42 ("Song").
async fn recovery_commands(position_ms: u64, started: Option<i64>) -> Vec<ResolumeCommand> {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, position_ms, started);
    sent(&mut rx);
    engine.handle_resolume_recovery("127.0.0.1").await;
    sent(&mut rx)
}

/// The commands a scene-on sends for playlist 7, playing video 42 ("Song")
/// while its scene was off program.
async fn scene_on_commands(position_ms: u64, started: Option<i64>) -> Vec<ResolumeCommand> {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, position_ms, started);
    engine.pipelines[&7]
        .scene_active
        .store(false, Ordering::Release);
    sent(&mut rx);
    engine.handle_scene_change(7, true).await;
    sent(&mut rx)
}

/// #45 — a scene becomes program for a pipeline already playing (its 1.5 s
/// show task found the scene off and showed nothing): mid-song, the title is
/// re-synced. It goes through the driver's `Resync` (#217 addendum 3), never
/// a ShowTitle, whose fade from 5 % blinked a title that was already up.
#[tokio::test]
async fn scene_go_on_refreshes_title_for_already_playing() {
    let cmds = scene_on_commands(60_000, Some(42)).await;
    assert_eq!(
        resyncs(&cmds),
        [Some("Song - Artist".to_string())],
        "scene-go-on mid-song re-syncs the title, got {cmds:?}"
    );
    assert!(!shows_title(&cmds), "no ShowTitle, got {cmds:?}");
}

/// Design record 5859883842 item 3: the scene-on re-push applied no title
/// window, so the wall showed a title outside it. A scene-on before the song
/// started (the scene-on itself selected it: the position and duration are
/// the last song's), in its first 1.5 s, or in its last 3.5 s shows none.
#[tokio::test]
async fn scene_go_on_outside_the_title_window_pushes_no_title() {
    for (position_ms, started, when) in [
        (60_000, Some(41), "before the song's Started"),
        (1_000, Some(42), "in the first 1.5 s"),
        (177_000, Some(42), "in the last 3.5 s"),
    ] {
        let cmds = scene_on_commands(position_ms, started).await;
        assert!(!shows_title(&cmds), "{when}: no ShowTitle, got {cmds:?}");
        assert_eq!(
            resyncs(&cmds),
            [None::<String>],
            "{when}: the re-sync names no title, got {cmds:?}"
        );
    }
}

/// On a RecoveryEvent the title of an on-program pipeline inside its title
/// window is re-synced.
#[tokio::test]
async fn handle_resolume_recovery_reemits_title_for_active_pipeline() {
    let cmds = recovery_commands(60_000, Some(42)).await;
    assert_eq!(
        resyncs(&cmds),
        [Some("Song - Artist".to_string())],
        "the title must be re-synced on Resolume recovery, got {cmds:?}"
    );
    assert!(!shows_title(&cmds), "no ShowTitle, got {cmds:?}");
}

/// Review round 2 of addendum 2: after the song's hide point (3.5 s before
/// its end) a recovery must not show the title again: it would stay into the
/// next song. The driver fires such a recovery when that very HideTitle got a
/// 404 (Arena re-ids its clips on relaunch) and the refresh mapped the new
/// ids; it retries the hide, and the re-sync must not undo it.
#[tokio::test]
async fn handle_resolume_recovery_does_not_re_show_a_title_the_song_end_hid() {
    let cmds = recovery_commands(SONG_MS - 3_500, Some(42)).await;
    assert_eq!(
        resyncs(&cmds),
        [None::<String>],
        "no title after the hide point, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, ResolumeCommand::HideSubtitles)),
        "the subtitle state is still re-sent, got {cmds:?}"
    );
}

/// The usual case, an Arena relaunch mid-song: the title is re-synced.
#[tokio::test]
async fn handle_resolume_recovery_re_shows_the_title_mid_song() {
    let cmds = recovery_commands(SONG_MS - 3_501, Some(42)).await;
    assert_eq!(
        resyncs(&cmds),
        [Some("Song - Artist".to_string())],
        "one ms before the hide point the title is due, got {cmds:?}"
    );
}

/// A recovery in the first 1.5 s of a song: the show timer shows the title
/// itself, so the re-sync names none yet.
#[tokio::test]
async fn handle_resolume_recovery_leaves_a_pending_title_to_its_show_timer() {
    let cmds = recovery_commands(1_499, Some(42)).await;
    assert_eq!(
        resyncs(&cmds),
        [None::<String>],
        "not due before 1.5 s, got {cmds:?}"
    );
    let cmds = recovery_commands(1_500, Some(42)).await;
    assert_eq!(
        resyncs(&cmds),
        [Some("Song - Artist".to_string())],
        "due from 1.5 s, got {cmds:?}"
    );
}

/// Design record 5859883842 root cause 2: a recovery between one song's end
/// and the next song's `Started` showed the next song's title early (both
/// timer handles were `None`, which read as "mid-song"). Until the new song's
/// `Started`, its title window stays closed.
#[tokio::test]
async fn handle_resolume_recovery_between_songs_shows_no_title() {
    let cmds = recovery_commands(60_000, Some(41)).await;
    assert_eq!(
        resyncs(&cmds),
        [None::<String>],
        "the next song has not started: no title, got {cmds:?}"
    );
    let cmds = recovery_commands(60_000, None).await;
    assert_eq!(
        resyncs(&cmds),
        [None::<String>],
        "no song started yet, got {cmds:?}"
    );
}

/// A program scene with two SongPlayer playlists: they share the one
/// `#sp-title` clip, so the re-sync names one title, the highest playlist
/// id's among those due, whatever the HashMap order.
#[tokio::test]
async fn a_recovery_with_two_playlists_on_program_resyncs_one_title() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song"), (9, 44, "Later")]).await;
    play(&mut engine, 7, 42, 60_000, Some(42));
    play(&mut engine, 9, 44, 60_000, Some(44));
    sent(&mut rx);

    engine.handle_resolume_recovery("127.0.0.1").await;
    assert_eq!(
        resyncs(&sent(&mut rx)),
        [Some("Later - Artist".to_string())],
        "both due: the highest playlist id's title"
    );

    play(&mut engine, 9, 44, 500, Some(44));
    engine.handle_resolume_recovery("127.0.0.1").await;
    assert_eq!(
        resyncs(&sent(&mut rx)),
        [Some("Song - Artist".to_string())],
        "only playlist 7's title is due"
    );
}

/// A song's `Started` opens its title window: the video is marked started
/// and its position restarts at 0 (the last song's position would otherwise
/// count). The first Position inside the window makes the title due.
#[tokio::test]
async fn started_opens_the_song_s_title_window() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, 170_000, Some(41));

    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::Started {
                duration_ms: SONG_MS,
            },
        )
        .await;
    let pp = &engine.pipelines[&7];
    assert_eq!(pp.started_video_id, Some(42), "the song is marked started");
    assert_eq!(pp.cached_position_ms, 0, "its position restarts");
    sent(&mut rx);
    engine.handle_resolume_recovery("127.0.0.1").await;
    assert_eq!(resyncs(&sent(&mut rx)), [None::<String>], "not due at 0 ms");

    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::Position {
                position_ms: 60_000,
                duration_ms: SONG_MS,
            },
        )
        .await;
    engine.handle_resolume_recovery("127.0.0.1").await;
    assert_eq!(
        resyncs(&sent(&mut rx)),
        [Some("Song - Artist".to_string())],
        "due mid-song"
    );
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}
