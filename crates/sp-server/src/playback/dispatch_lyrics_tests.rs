//! Tests for `PlaybackEngine::dispatch_lyrics_if_changed`.
//! Sibling-included from playback/mod.rs (see `#[cfg(test)] mod` declaration).

#![allow(unused_imports)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sp_core::lyrics::{LyricsLine, LyricsTrack};
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

use super::*;
use crate::lyrics::renderer::LyricsState;
use crate::playback::ndi_health::NdiHealthRegistry;
use crate::playback::pipeline::PlaybackPipeline;

fn make_track() -> LyricsTrack {
    LyricsTrack {
        version: 20,
        source: "test".into(),
        language_source: "en".into(),
        language_translation: "sk".into(),
        lines: vec![
            LyricsLine {
                start_ms: 1000,
                end_ms: 3000,
                en: "alpha".into(),
                sk: Some("alfa".into()),
                words: None,
            },
            LyricsLine {
                start_ms: 4000,
                end_ms: 6000,
                en: "beta".into(),
                sk: Some("beta".into()),
                words: None,
            },
            LyricsLine {
                start_ms: 7000,
                end_ms: 9000,
                en: "gamma".into(),
                sk: Some("gama".into()),
                words: None,
            },
        ],
    }
}

fn build_engine() -> (
    PlaybackEngine,
    mpsc::Receiver<crate::resolume::ResolumeCommand>,
    broadcast::Receiver<ServerMsg>,
) {
    let pool = futures::executor::block_on(async {
        let p = crate::db::create_memory_pool().await.unwrap();
        crate::db::run_migrations(&p).await.unwrap();
        p
    });
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, resolume_rx) = mpsc::channel(16);
    let (ws_tx, ws_rx) = broadcast::channel::<ServerMsg>(16);
    let engine = PlaybackEngine::new(
        pool,
        std::path::PathBuf::from("/tmp/test-cache"),
        obs_tx,
        None,
        resolume_tx,
        ws_tx,
        None, // presenter_client = None: tests don't assert presenter HTTP push.
        Arc::new(NdiHealthRegistry::new()),
    );
    (engine, resolume_rx, ws_rx)
}

fn install_pipeline(
    engine: &mut PlaybackEngine,
    playlist_id: i64,
    scene_active: bool,
    lyrics: Option<LyricsState>,
) {
    let pipeline = PlaybackPipeline::spawn(
        format!("test-{playlist_id}"),
        None,
        mpsc::unbounded_channel().0,
        playlist_id,
    );
    let pp = PlaylistPipeline {
        pipeline,
        state: PlayState::Idle,
        mode: PlaybackMode::default(),
        current_video_id: Some(42),
        scene_active: Arc::new(AtomicBool::new(scene_active)),
        title_show_abort: None,
        title_hide_abort: None,
        cached_song: "Song".into(),
        cached_artist: "Artist".into(),
        cached_duration_ms: 10_000,
        cached_suppress_en: false,
        last_now_playing_broadcast: None,
        history: VecDeque::new(),
        lyrics_state: lyrics,
        last_presenter_text: None,
        last_resolume_subtitles_signature: None,
        last_lyrics_ws_signature: None,
        cached_position_ms: 0,
    };
    engine.pipelines.insert(playlist_id, pp);
}

#[tokio::test]
async fn dispatch_lyrics_skips_when_no_lyrics_state() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, None);

    engine.dispatch_lyrics_if_changed(99, 1500);

    assert!(
        resolume_rx.try_recv().is_err(),
        "no lyrics_state → no Resolume command"
    );
    assert!(ws_rx.try_recv().is_err(), "no lyrics_state → no ws message");
}

#[tokio::test]
async fn dispatch_lyrics_fires_on_first_position_event() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, Some(LyricsState::new(make_track())));

    engine.dispatch_lyrics_if_changed(99, 1500); // inside line "alpha" 1000..3000

    let cmd = resolume_rx
        .try_recv()
        .expect("Resolume ShowSubtitles must fire on first event");
    match cmd {
        crate::resolume::ResolumeCommand::ShowSubtitles { en, .. } => {
            assert!(en.contains("alpha"), "got: {en}");
        }
        other => panic!("expected ShowSubtitles, got {other:?}"),
    }

    let msg = ws_rx
        .try_recv()
        .expect("ws LyricsUpdate must fire on first event");
    match msg {
        ServerMsg::LyricsUpdate {
            line_en,
            playlist_id,
            ..
        } => {
            assert_eq!(playlist_id, 99);
            assert_eq!(line_en.as_deref(), Some("alpha"));
        }
        other => panic!("expected LyricsUpdate, got {other:?}"),
    }

    let pp = engine.pipelines.get(&99).unwrap();
    assert!(pp.last_resolume_subtitles_signature.is_some());
    assert_eq!(pp.last_lyrics_ws_signature.as_deref(), Some("alpha"));
}

#[tokio::test]
async fn dispatch_lyrics_idempotent_on_same_line() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, Some(LyricsState::new(make_track())));

    // First call inside "alpha" line: fires.
    engine.dispatch_lyrics_if_changed(99, 1500);
    let _first_resolume = resolume_rx.try_recv().expect("first call fires Resolume");
    let _first_ws = ws_rx.try_recv().expect("first call fires ws");

    // Second call still inside "alpha": NO fires.
    engine.dispatch_lyrics_if_changed(99, 2200);
    assert!(
        resolume_rx.try_recv().is_err(),
        "same line → no second Resolume command"
    );
    assert!(
        ws_rx.try_recv().is_err(),
        "same line → no second ws message"
    );
}

#[tokio::test]
async fn dispatch_lyrics_fires_on_line_change() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, Some(LyricsState::new(make_track())));

    // First inside "alpha" 1000..3000.
    engine.dispatch_lyrics_if_changed(99, 1500);
    let _ = resolume_rx.try_recv().unwrap();
    let _ = ws_rx.try_recv().unwrap();

    // Second inside "beta" 4000..6000.
    engine.dispatch_lyrics_if_changed(99, 4500);

    let cmd = resolume_rx
        .try_recv()
        .expect("line change → second Resolume command");
    match cmd {
        crate::resolume::ResolumeCommand::ShowSubtitles { en, .. } => {
            assert!(en.contains("beta"), "got: {en}");
        }
        other => panic!("expected ShowSubtitles, got {other:?}"),
    }
    let msg = ws_rx.try_recv().expect("line change → second ws message");
    match msg {
        ServerMsg::LyricsUpdate { line_en, .. } => {
            assert_eq!(line_en.as_deref(), Some("beta"));
        }
        other => panic!("expected LyricsUpdate, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_lyrics_resolume_gated_on_scene_active() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(
        &mut engine,
        99,
        false, // scene_active = false: Resolume must NOT fire
        Some(LyricsState::new(make_track())),
    );

    engine.dispatch_lyrics_if_changed(99, 1500);

    assert!(
        resolume_rx.try_recv().is_err(),
        "scene_active=false → no Resolume command"
    );
    let msg = ws_rx
        .try_recv()
        .expect("ws LyricsUpdate must still fire when scene_active=false");
    match msg {
        ServerMsg::LyricsUpdate { line_en, .. } => {
            assert_eq!(line_en.as_deref(), Some("alpha"));
        }
        other => panic!("expected LyricsUpdate, got {other:?}"),
    }
}

#[tokio::test]
async fn dispatch_lyrics_no_throttle() {
    let (mut engine, mut resolume_rx, mut ws_rx) = build_engine();
    install_pipeline(&mut engine, 99, true, Some(LyricsState::new(make_track())));

    // Two events 100 ms apart on DIFFERENT lines must both fire — proves the
    // 500 ms position-update throttle does NOT gate this dispatch path.
    engine.dispatch_lyrics_if_changed(99, 1500); // alpha
    let _ = resolume_rx.try_recv().unwrap();
    let _ = ws_rx.try_recv().unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    engine.dispatch_lyrics_if_changed(99, 4500); // beta

    let cmd = resolume_rx
        .try_recv()
        .expect("second event must fire 100 ms later — no throttle");
    matches!(cmd, crate::resolume::ResolumeCommand::ShowSubtitles { .. });
    let _ = ws_rx
        .try_recv()
        .expect("second ws message must fire 100 ms later — no throttle");
}
