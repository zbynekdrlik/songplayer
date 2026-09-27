//! Regression tests for the 2026-04-19 event cross-playlist Resolume
//! bleed bug. Sibling file so `playback/mod.rs` and `playback/tests.rs`
//! each stay under the 1000-line airuleset cap.

#![allow(unused_imports)]

use std::sync::atomic::Ordering;

use super::pipeline::PipelineEvent;
use super::title::TitleClock;
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

// -- #217 addendum 3: the wall title is re-synced, per the song's title clock --
//
// A recovery and an OBS scene-on send the driver ONE `Resync` naming the title
// that SHOULD be up: the song title of a playing, on-program pipeline whose
// title clock (fixed at its `Started`, the instants the title timers sleep
// until) is open, else none. The driver owns the wall and acts only on a
// difference (`resolume/driver_title_tests.rs`).

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

/// Where a song is in its title window, as its title clock says.
#[derive(Clone, Copy, Debug)]
enum Window {
    /// Past the show point, before the hide point.
    Due,
    /// Before the show point (the song's first 1.5 s).
    BeforeShow,
    /// Past the hide point (the song's last 3.5 s).
    AfterHide,
    /// The clock is the previous song's: this one has not had its `Started`.
    OtherSong,
    /// No song has started on this playlist yet.
    NotStarted,
}

/// The title clock of `video_id` for `window`. Its instants sit at the
/// test's clock or an hour away, never a subtraction from it (a freshly
/// booted Windows runner's monotonic clock can underflow).
fn clock_for(video_id: i64, window: Window) -> Option<TitleClock> {
    let now = tokio::time::Instant::now();
    let hour = std::time::Duration::from_secs(3600);
    let clock = |video_id, show_at, hide_at| TitleClock {
        video_id,
        show_at,
        hide_at: Some(hide_at),
    };
    match window {
        Window::Due => Some(clock(video_id, now, now + hour)),
        Window::BeforeShow => Some(clock(video_id, now + hour, now + 2 * hour)),
        Window::AfterHide => Some(clock(video_id, now, now)),
        Window::OtherSong => Some(clock(video_id - 1, now, now + hour)),
        Window::NotStarted => None,
    }
}

/// The decoder position a song in `window` reports (consistent with the
/// clock; a lagging report is set by the test that means it).
fn position_for(window: Window) -> u64 {
    match window {
        Window::BeforeShow => 1_000,
        Window::AfterHide => SONG_MS - 3_000,
        Window::Due | Window::OtherSong | Window::NotStarted => 60_000,
    }
}

/// Playlist `playlist_id` plays `video_id` on program, at `window` of its
/// `SONG_MS`, with no title timers armed.
fn play(engine: &mut PlaybackEngine, playlist_id: i64, video_id: i64, window: Window) {
    let pp = engine.pipelines.get_mut(&playlist_id).expect("pipeline");
    pp.state = PlayState::Playing { video_id };
    pp.current_video_id = Some(video_id);
    pp.title_clock = clock_for(video_id, window);
    pp.cached_position_ms = position_for(window);
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
async fn recovery_commands(window: Window) -> Vec<ResolumeCommand> {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, window);
    sent(&mut rx);
    engine.handle_resolume_recovery("127.0.0.1").await;
    sent(&mut rx)
}

/// The engine after a scene-on of playlist 7, playing video 42 ("Song")
/// while its scene was off program (its title timers cancelled, as a
/// scene-off leaves them), and the commands it sent.
async fn after_scene_on(window: Window) -> (PlaybackEngine, Vec<ResolumeCommand>) {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, window);
    engine.pipelines[&7]
        .scene_active
        .store(false, Ordering::Release);
    sent(&mut rx);
    engine.handle_scene_change(7, true).await;
    let cmds = sent(&mut rx);
    (engine, cmds)
}

/// Whether playlist 7 has a show / hide timer armed.
fn timers(engine: &PlaybackEngine) -> (bool, bool) {
    let pp = &engine.pipelines[&7];
    (pp.title_show_abort.is_some(), pp.title_hide_abort.is_some())
}

/// #45 — a scene becomes program for a pipeline already playing (its 1.5 s
/// show task found the scene off and showed nothing): mid-song, the title is
/// re-synced. It goes through the driver's `Resync` (#217 addendum 3), never
/// a ShowTitle, whose fade from 5 % blinked a title that was already up.
#[tokio::test]
async fn scene_go_on_refreshes_title_for_already_playing() {
    let (_engine, cmds) = after_scene_on(Window::Due).await;
    assert_eq!(
        resyncs(&cmds),
        [Some("Song - Artist".to_string())],
        "scene-go-on mid-song re-syncs the title, got {cmds:?}"
    );
    assert!(!shows_title(&cmds), "no ShowTitle, got {cmds:?}");
}

/// Design record 5859883842 item 3: the scene-on re-push applied no title
/// window, so the wall showed a title outside it. A scene-on before the
/// song's `Started` (the scene-on itself selected it), in its first 1.5 s, or
/// in its last 3.5 s shows none.
#[tokio::test]
async fn scene_go_on_outside_the_title_window_pushes_no_title() {
    for window in [Window::OtherSong, Window::BeforeShow, Window::AfterHide] {
        let (_engine, cmds) = after_scene_on(window).await;
        assert!(
            !shows_title(&cmds),
            "{window:?}: no ShowTitle, got {cmds:?}"
        );
        assert_eq!(
            resyncs(&cmds),
            [None::<String>],
            "{window:?}: the re-sync names no title, got {cmds:?}"
        );
    }
}

/// Review round 1 (🟡 3): a scene-off cancels the song's title timers, and
/// the #215 transition hold keeps the song playing. A scene-on then arms
/// them again from the song's clock, for what is still ahead: before, a
/// bounce in the first 1.5 s left the song with no title, and a later one
/// with no hide 3.5 s before the end. A clock of another song arms nothing
/// (that song's `Started` will).
#[tokio::test]
async fn a_scene_on_rearms_the_song_s_title_timers_for_what_is_ahead() {
    for (window, expected) in [
        (Window::BeforeShow, (true, true)),
        (Window::Due, (false, true)),
        (Window::AfterHide, (false, false)),
        (Window::OtherSong, (false, false)),
        (Window::NotStarted, (false, false)),
    ] {
        let (mut engine, _cmds) = after_scene_on(window).await;
        assert_eq!(
            timers(&engine),
            expected,
            "{window:?}: (show, hide) timers armed"
        );
        engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
    }
}

/// The timers are armed only for an instant still AHEAD: at the show or
/// hide instant itself, the recovery's clock already says so.
#[tokio::test]
async fn title_timers_are_armed_only_for_instants_still_ahead() {
    let (mut engine, _rx) = test_engine(&[(7, 42, "Song")]).await;
    let now = tokio::time::Instant::now();
    let hour = std::time::Duration::from_secs(3600);
    let pp = engine.pipelines.get_mut(&7).unwrap();
    pp.title_clock = Some(TitleClock {
        video_id: 42,
        show_at: now,
        hide_at: Some(now),
    });
    engine.arm_title_timers(7, now);
    assert_eq!(timers(&engine), (false, false), "at the instants: none");

    engine.pipelines.get_mut(&7).unwrap().title_clock = Some(TitleClock {
        video_id: 42,
        show_at: now + hour,
        hide_at: Some(now + hour),
    });
    engine.arm_title_timers(7, now);
    assert_eq!(timers(&engine), (true, true), "both ahead: both armed");
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}

/// On a RecoveryEvent the title of an on-program pipeline inside its title
/// window is re-synced.
#[tokio::test]
async fn handle_resolume_recovery_reemits_title_for_active_pipeline() {
    let cmds = recovery_commands(Window::Due).await;
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
    let cmds = recovery_commands(Window::AfterHide).await;
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
    let cmds = recovery_commands(Window::Due).await;
    assert_eq!(
        resyncs(&cmds),
        [Some("Song - Artist".to_string())],
        "mid-song the title is due, got {cmds:?}"
    );
}

/// A recovery in the first 1.5 s of a song: the show timer shows the title
/// itself, so the re-sync names none yet.
#[tokio::test]
async fn handle_resolume_recovery_leaves_a_pending_title_to_its_show_timer() {
    let cmds = recovery_commands(Window::BeforeShow).await;
    assert_eq!(
        resyncs(&cmds),
        [None::<String>],
        "not due before the show point, got {cmds:?}"
    );
}

/// Review round 1 (🔴 1): the recovery read the decoder position, reported
/// every 500 ms, while the timers run on the clock of the song's `Started`.
/// Near a boundary the two disagreed: a Resync naming the title just after
/// the hide timer's HideTitle superseded it (the title stayed into the next
/// song), a Resync naming none just after the show timer's ShowTitle hid it
/// (no title for the song). The re-sync now reads the timers' own clock, not
/// the last position report.
#[tokio::test]
async fn a_recovery_follows_the_title_clock_not_a_lagging_position() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    engine.pipelines.get_mut(&7).unwrap().cached_position_ms = 1_200;
    engine.handle_resolume_recovery("127.0.0.1").await;
    assert_eq!(
        resyncs(&sent(&mut rx)),
        [Some("Song - Artist".to_string())],
        "past the show instant, whatever the last report (1.2 s) says"
    );

    play(&mut engine, 7, 42, Window::AfterHide);
    engine.pipelines.get_mut(&7).unwrap().cached_position_ms = SONG_MS - 4_000;
    engine.handle_resolume_recovery("127.0.0.1").await;
    assert_eq!(
        resyncs(&sent(&mut rx)),
        [None::<String>],
        "past the hide instant, whatever the last report (4 s before the end) says"
    );
}

/// A song playing off program (its scene is not on the wall, e.g. held
/// through a transition) has no title due, whatever its clock. Kills the
/// `on_program || (clock due)` mutant of `title_due`.
#[tokio::test]
async fn handle_resolume_recovery_names_no_title_for_an_off_program_song() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::Due);
    engine.pipelines[&7]
        .scene_active
        .store(false, Ordering::Release);
    sent(&mut rx);

    engine.handle_resolume_recovery("127.0.0.1").await;

    assert_eq!(
        resyncs(&sent(&mut rx)),
        [None::<String>],
        "off program: no title"
    );
}

/// Design record 5859883842 root cause 2: a recovery between one song's end
/// and the next song's `Started` showed the next song's title early (both
/// timer handles were `None`, which read as "mid-song"). The clock is still
/// the previous song's until the new one's `Started`, so the window stays
/// closed.
#[tokio::test]
async fn handle_resolume_recovery_between_songs_shows_no_title() {
    for window in [Window::OtherSong, Window::NotStarted] {
        let cmds = recovery_commands(window).await;
        assert_eq!(
            resyncs(&cmds),
            [None::<String>],
            "{window:?}: the next song has not started, got {cmds:?}"
        );
    }
}

/// A program scene with two SongPlayer playlists: they share the one
/// `#sp-title` clip, so the re-sync names one title, the highest playlist
/// id's among those due, whatever the HashMap order.
#[tokio::test]
async fn a_recovery_with_two_playlists_on_program_resyncs_one_title() {
    let (mut engine, mut rx) = test_engine(&[(7, 42, "Song"), (9, 44, "Later")]).await;
    play(&mut engine, 7, 42, Window::Due);
    play(&mut engine, 9, 44, Window::Due);
    sent(&mut rx);

    engine.handle_resolume_recovery("127.0.0.1").await;
    assert_eq!(
        resyncs(&sent(&mut rx)),
        [Some("Later - Artist".to_string())],
        "both due: the highest playlist id's title"
    );

    play(&mut engine, 9, 44, Window::BeforeShow);
    engine.handle_resolume_recovery("127.0.0.1").await;
    assert_eq!(
        resyncs(&sent(&mut rx)),
        [Some("Song - Artist".to_string())],
        "only playlist 7's title is due"
    );
}

/// A song's `Started` fixes its title clock (show 1.5 s after it, hide 3.5 s
/// before the end) and arms both timers from it. It no longer resets the
/// cached position: a Pause before the first Position report kept 0 and
/// resumed the song from its start (review round 1, 🔵 4).
#[tokio::test]
async fn started_fixes_the_song_s_title_clock_and_arms_its_timers() {
    let (mut engine, _rx) = test_engine(&[(7, 42, "Song")]).await;
    play(&mut engine, 7, 42, Window::OtherSong);
    engine.pipelines.get_mut(&7).unwrap().cached_position_ms = 85_240;

    engine
        .handle_pipeline_event(
            7,
            PipelineEvent::Started {
                duration_ms: SONG_MS,
            },
        )
        .await;

    let pp = &engine.pipelines[&7];
    let clock = pp.title_clock.expect("the song's clock");
    assert_eq!(clock.video_id, 42);
    assert_eq!(
        clock.hide_at.expect("a 180 s song hides") - clock.show_at,
        std::time::Duration::from_millis(SONG_MS - 5_000),
        "shown at +1.5 s, hidden 3.5 s before the end"
    );
    assert_eq!(timers(&engine), (true, true), "both timers armed");
    assert_eq!(
        engine.pipelines[&7].cached_position_ms, 85_240,
        "the position is the pipeline's to report"
    );
    engine.pipelines.get_mut(&7).unwrap().cancel_title_timers();
}
