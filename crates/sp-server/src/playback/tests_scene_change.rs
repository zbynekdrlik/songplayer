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
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );

    engine.ensure_pipeline(7, "SP-fast");
    // Force the pipeline into scene_active = true so the transition
    // downward is what we're measuring.
    if let Some(pp) = engine.pipelines.get_mut(&7) {
        pp.scene_active = true;
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
    let mut engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
    );

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
