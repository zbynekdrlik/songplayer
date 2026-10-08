//! #147: a song that starts on air marks the program trace — the engine's
//! `Started` arm (`trace_song_start`), read through the record the sender
//! writes next for that playlist. Off air or paused, nothing is marked.
//! Wired via `#[cfg(test)] #[path = "handle_pipeline_event_tests_trace.rs"]
//! mod tests_trace;` in `handle_pipeline_event.rs`.

use std::sync::Arc;

use sp_core::genlock::{GENLOCK_GRID_FPS, grid_boundary_100ns};
use tokio::sync::{broadcast, mpsc};

use crate::playback::pipeline::PipelineEvent;
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_output_timing::BoundaryMarks;
use crate::playback::program_trace::{JobShape, TraceKind};
use crate::playback::state::PlayState;
use crate::playback::{PlaybackEngine, PlaybackEngineConfig};

const PLAYLIST: i64 = 7;
const SONG: i64 = 42;

/// An engine with `PLAYLIST` playing `SONG`, and the program bus it marks.
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
         '/tmp/sp-trace-42_video.mp4', '/tmp/sp-trace-42_audio.flac')",
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
        cache_dir: std::path::PathBuf::from("/tmp/test-cache-trace"),
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

async fn started(engine: &mut PlaybackEngine) {
    let event = PipelineEvent::Started {
        duration_ms: 180_000,
        position_ms: 0,
    };
    engine.handle_pipeline_event(PLAYLIST, event).await;
}

/// The song the sender's next live boundary of `PLAYLIST` carries.
fn next_boundary_song(bus: &ProgramBus) -> Option<i64> {
    let trace = bus.trace();
    let mut writer = trace.writer().expect("the trace's writer");
    let stamp = grid_boundary_100ns(1_759_882_400 * GENLOCK_GRID_FPS, GENLOCK_GRID_FPS);
    let marks = BoundaryMarks {
        stamp_100ns: stamp,
        taken_100ns: stamp,
        fed_100ns: stamp,
        submit_start_100ns: stamp,
        submitted_100ns: stamp,
    };
    let live = JobShape {
        kind: TraceKind::Source,
        live: true,
    };
    writer.record(&marks, Some(PLAYLIST), live, 0);
    let written = trace.written() - 1;
    trace.read(written).and_then(|r| r.song)
}

#[tokio::test]
async fn a_song_that_starts_on_air_is_marked_on_the_program_trace() {
    let (mut engine, bus) = rig().await;
    engine.put_on_air_for_test(PLAYLIST);
    started(&mut engine).await;
    assert_eq!(next_boundary_song(&bus), Some(SONG));
}

#[tokio::test]
async fn a_song_that_starts_off_air_is_not_marked() {
    let (mut engine, bus) = rig().await;
    started(&mut engine).await;
    assert_eq!(next_boundary_song(&bus), None);
}

/// A `Started` that a pause overtook: its song is not playing, so it is not
/// marked (its resume's `Started` will be).
#[tokio::test]
async fn a_song_paused_before_it_started_is_not_marked() {
    let (mut engine, bus) = rig().await;
    engine.put_on_air_for_test(PLAYLIST);
    engine.pipelines.get_mut(&PLAYLIST).unwrap().paused_at = Some((SONG, 0));
    started(&mut engine).await;
    assert_eq!(next_boundary_song(&bus), None);
}
