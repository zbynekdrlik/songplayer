//! #228: a play's media clock is marked on the program's item record — the
//! engine's `Started` (the play's real start) and `Seeked` (the pts start
//! again) arms, on air or not. Wired via `#[cfg(test)] #[path =
//! "handle_pipeline_event_tests_item.rs"] mod tests_item;` in
//! `handle_pipeline_event.rs`.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use crate::playback::pipeline::PipelineEvent;
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_item::ItemMark;
use crate::playback::state::PlayState;
use crate::playback::{PlaybackEngine, PlaybackEngineConfig};

const PLAYLIST: i64 = 7;
const SONG: i64 = 42;

/// An engine with `PLAYLIST` playing `SONG` (off air), and its program bus.
async fn rig() -> (PlaybackEngine, Arc<ProgramBus>) {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO playlists (id, name, youtube_url, ndi_output_name, is_active) \
         VALUES (?, 'p', 'u', 'SP-7', 1)",
    )
    .bind(PLAYLIST)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO videos (id, playlist_id, youtube_id, song, artist, normalized, \
         file_path, audio_file_path) VALUES (?, ?, 'yt42', 'Song', 'Artist', 1, \
         '/tmp/sp-item-42_video.mp4', '/tmp/sp-item-42_audio.flac')",
    )
    .bind(SONG)
    .bind(PLAYLIST)
    .execute(&pool)
    .await
    .unwrap();
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _resolume) = mpsc::channel(64);
    let (ws_tx, _) = broadcast::channel(256);
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache-item"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    });
    engine.ensure_pipeline(PLAYLIST, "SP-7");
    let pp = engine.pipelines.get_mut(&PLAYLIST).expect("the pipeline");
    pp.state = PlayState::Playing { video_id: SONG };
    pp.current_video_id = Some(SONG);
    let bus = Arc::new(ProgramBus::new());
    assert!(engine.program.set(bus.clone()).is_ok());
    (engine, bus)
}

#[tokio::test]
async fn a_started_play_marks_its_video_and_its_real_start() {
    let (mut engine, bus) = rig().await;
    let started = PipelineEvent::Started {
        duration_ms: 128_000,
        position_ms: 0,
    };
    engine.handle_pipeline_event(PLAYLIST, started).await;
    assert_eq!(
        bus.item().mark_of(PLAYLIST),
        Some(ItemMark {
            seq: 1,
            video_id: SONG,
            start_ms: 0,
        }),
        "marked off air too: the item record follows every play"
    );
    let resumed = PipelineEvent::Started {
        duration_ms: 128_000,
        position_ms: 60_000,
    };
    engine.handle_pipeline_event(PLAYLIST, resumed).await;
    assert_eq!(
        bus.item().mark_of(PLAYLIST).map(|m| (m.seq, m.start_ms)),
        Some((2, 60_000))
    );
}

#[tokio::test]
async fn a_seek_marks_where_the_pts_start_again() {
    let (mut engine, bus) = rig().await;
    let seeked = PipelineEvent::Seeked {
        position_ms: 30_000,
    };
    engine.handle_pipeline_event(PLAYLIST, seeked).await;
    assert_eq!(
        bus.item().mark_of(PLAYLIST),
        Some(ItemMark {
            seq: 1,
            video_id: SONG,
            start_ms: 30_000,
        })
    );
}

#[tokio::test]
async fn a_playlist_with_no_current_video_marks_nothing() {
    let (mut engine, bus) = rig().await;
    engine
        .pipelines
        .get_mut(&PLAYLIST)
        .unwrap()
        .current_video_id = None;
    let seeked = PipelineEvent::Seeked {
        position_ms: 30_000,
    };
    engine.handle_pipeline_event(PLAYLIST, seeked).await;
    assert_eq!(bus.item().mark_of(PLAYLIST), None);
}
