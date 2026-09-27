//! Regression tests for the 2026-04-19 event cross-playlist Resolume
//! bleed bug. Sibling file so `playback/mod.rs` and `playback/tests.rs`
//! each stay under the 1000-line airuleset cap.

#![allow(unused_imports)]

use super::*;
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

/// #45 — when a scene becomes program for a pipeline that is already in
/// `Playing` state (off-program), `handle_scene_change` MUST re-push the
/// title to Resolume so the wall doesn't keep showing the previous song.
/// The 1.5s post-Started title-show task aborted itself with "title
/// suppressed — off program"; nothing else re-pushes title without this fix.
#[tokio::test]
async fn scene_go_on_refreshes_title_for_already_playing() {
    use std::sync::atomic::Ordering;

    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();

    // Parent playlist row (FK target for videos.playlist_id).
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (7, 'test', 'https://example.com/p', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Video row that get_video_title_info will resolve.
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized) \
         VALUES (42, 7, 'abc123', 'Test Song', 'Test Artist', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

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
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.state = PlayState::Playing { video_id: 42 };
        pp.scene_active.store(false, Ordering::Release);
    }

    // Drain any residual messages from setup.
    while resolume_rx.try_recv().is_ok() {}

    // Scene becomes program.
    engine.handle_scene_change(7, true).await;

    // Collect every ResolumeCommand emitted during the call (the helper
    // pushes ShowTitle but earlier scene-state code might also emit other
    // messages — we look for the ShowTitle specifically).
    let mut cmds: Vec<crate::resolume::ResolumeCommand> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(50), resolume_rx.recv()).await {
            Ok(Some(cmd)) => cmds.push(cmd),
            Ok(None) => break,
            Err(_) => {
                if !cmds.is_empty() {
                    break;
                }
            }
        }
    }

    let show_title = cmds.iter().find_map(|c| match c {
        crate::resolume::ResolumeCommand::ShowTitle { song, artist } => {
            Some((song.clone(), artist.clone()))
        }
        _ => None,
    });
    assert_eq!(
        show_title,
        Some(("Test Song".into(), "Test Artist".into())),
        "scene-go-on for an already-Playing pipeline MUST re-push ShowTitle. Got: {cmds:?}"
    );
}

/// On RecoveryEvent, handle_resolume_recovery MUST re-emit ShowTitle for every
/// active pipeline in Playing state with scene_active=true.
#[tokio::test]
async fn handle_resolume_recovery_reemits_title_for_active_pipeline() {
    use std::sync::atomic::Ordering;

    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (7, 'p', 'u', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized) \
         VALUES (42, 7, 'abc', 'Song', 'Artist', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

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
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.state = PlayState::Playing { video_id: 42 };
        pp.scene_active.store(true, Ordering::Release);
    }
    while resolume_rx.try_recv().is_ok() {}

    engine.handle_resolume_recovery("127.0.0.1").await;

    let mut got_title = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(20), resolume_rx.recv()).await {
            Ok(Some(cmd)) => {
                if matches!(cmd, crate::resolume::ResolumeCommand::ShowTitle { .. }) {
                    got_title = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                if got_title {
                    break;
                }
            }
        }
    }
    assert!(
        got_title,
        "ShowTitle must be re-emitted on Resolume recovery"
    );
}

// -- #217 addendum 2: the recovery re-shows the title only in its window ----

/// An engine whose playlist 7 plays video 42 ("Song" / "Artist") on program,
/// with the title timers given (`None` = no timer). Returns the Resolume
/// commands one recovery sends.
async fn recovery_title_commands(
    show: Option<tokio::task::AbortHandle>,
    hide: Option<tokio::task::AbortHandle>,
) -> Vec<crate::resolume::ResolumeCommand> {
    use std::sync::atomic::Ordering;

    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (7, 'p', 'u', 'SP-fast', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized) \
         VALUES (42, 7, 'abc', 'Song', 'Artist', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
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
    let pp = engine.pipelines.get_mut(&7).expect("pipeline 7");
    pp.state = PlayState::Playing { video_id: 42 };
    pp.scene_active.store(true, Ordering::Release);
    pp.title_show_abort = show;
    pp.title_hide_abort = hide;
    while resolume_rx.try_recv().is_ok() {}

    engine.handle_resolume_recovery("127.0.0.1").await;

    let mut cmds = Vec::new();
    while let Ok(cmd) = resolume_rx.try_recv() {
        cmds.push(cmd);
    }
    cmds
}

/// A timer task that has already run.
async fn finished_timer() -> tokio::task::AbortHandle {
    let task = tokio::spawn(async {});
    let handle = task.abort_handle();
    task.await.unwrap();
    handle
}

/// A timer task that has not run yet (never completes on its own).
fn pending_timer() -> tokio::task::AbortHandle {
    tokio::spawn(std::future::pending::<()>()).abort_handle()
}

fn shows_title(cmds: &[crate::resolume::ResolumeCommand]) -> bool {
    cmds.iter()
        .any(|c| matches!(c, crate::resolume::ResolumeCommand::ShowTitle { .. }))
}

/// Review round 2: the song's end-of-song title hide has already run (its
/// timer task finished). A Resolume recovery then must not fade the title
/// back in for the song's last seconds and on into the next song. The driver
/// fires such a recovery when that very HideTitle got a 404 (Arena re-ids its
/// clips on relaunch) and the refresh mapped the new ids; it retries the
/// hide, and the re-push must not undo it.
#[tokio::test]
async fn handle_resolume_recovery_does_not_re_show_a_title_the_song_end_hid() {
    let cmds =
        recovery_title_commands(Some(finished_timer().await), Some(finished_timer().await)).await;
    assert!(
        !shows_title(&cmds),
        "a title the song end already hid is not re-shown, got {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, crate::resolume::ResolumeCommand::HideSubtitles)),
        "the subtitle state is still re-sent, got {cmds:?}"
    );
}

/// Review round 3: the usual case, an Arena relaunch mid-song. The title was
/// shown (the show timer ran) and the end-of-song hide is still pending: the
/// recovery re-shows the title.
#[tokio::test]
async fn handle_resolume_recovery_re_shows_the_title_mid_song() {
    let hide = pending_timer();
    let cmds = recovery_title_commands(Some(finished_timer().await), Some(hide.clone())).await;
    hide.abort();
    assert!(
        shows_title(&cmds),
        "mid-song the title is re-shown, got {cmds:?}"
    );
}

/// Review round 3: a recovery in the first 1.5 s of a song (e.g. the
/// song-start subtitle clear got a 404 after a relaunch in the song gap).
/// The pending show timer shows the title itself; a ShowTitle from the
/// recovery too would run the fade twice, a blink.
#[tokio::test]
async fn handle_resolume_recovery_leaves_a_pending_title_to_its_show_timer() {
    let (show, hide) = (pending_timer(), pending_timer());
    let cmds = recovery_title_commands(Some(show.clone()), Some(hide.clone())).await;
    show.abort();
    hide.abort();
    assert!(
        !shows_title(&cmds),
        "the show timer shows the title, got {cmds:?}"
    );
}
