//! #221 L4b: the playback authority — the task over a real `ProgramBus` (its
//! on-air watch and `legacy_cg`), and the engine's stale check over an
//! in-memory DB. Wired via `#[cfg(test)] #[path =
//! "program_authority_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use sp_core::config::PROGRAM_INPUT_ID;
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

use super::*;
use crate::playback::pipeline::PipelineEvent;
use crate::playback::program_bus::ProgramBus;
use crate::playback::state::PlayState;
use crate::playback::wallclock::utc_now_100ns;
use crate::playback::{PlaybackEngine, PlaybackEngineConfig};

type Events = mpsc::UnboundedReceiver<(i64, PipelineEvent)>;

/// The authority task over `bus`, and its events and shutdown.
fn authority(
    bus: &Arc<ProgramBus>,
) -> (Events, broadcast::Sender<()>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (shutdown, _) = broadcast::channel(1);
    let task = tokio::spawn(run_program_authority(
        Arc::clone(bus),
        tx,
        shutdown.subscribe(),
    ));
    (rx, shutdown, task)
}

/// The next `n` events (each within 10 s), as `(playlist, on)`.
async fn next(events: &mut Events, n: usize) -> Vec<(i64, bool)> {
    let mut got = Vec::new();
    for _ in 0..n {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("an on-program event within 10 s")
            .expect("the channel is open");
        match event {
            (pid, PipelineEvent::OnProgram(on)) => got.push((pid, on)),
            (pid, other) => panic!("playlist {pid}: not an on-program event: {other:?}"),
        }
    }
    got
}

/// Nothing more was sent: the task handled every change it was woken for
/// (it is idle again once a yield lets it run).
async fn nothing_more(events: &mut Events) {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(events.try_recv().is_err(), "no further event");
}

fn cg_shows(bus: &ProgramBus, shown: Option<i64>) {
    let legacy = bus.legacy_cg();
    let ticket = legacy.ticket();
    assert!(legacy.confirmed(ticket, shown));
}

/// The first value plays the restored program; a press is ON for the new
/// playlist while cg OBS still shows the old one (both on air until the
/// mirror's OK), then OFF for the old one; a press of the same scene
/// re-kicks it.
#[tokio::test]
async fn the_authority_plays_the_restored_program_and_follows_every_change() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(7, Some("sp-fast"));
    bus.legacy_cg().restored(7);
    let (mut events, _shutdown, _task) = authority(&bus);
    assert_eq!(
        next(&mut events, 1).await,
        [(7, true)],
        "the restored program"
    );
    nothing_more(&mut events).await;

    bus.cut(4, utc_now_100ns(), Some("sp-slow"));
    assert_eq!(
        next(&mut events, 2).await,
        [(4, true), (7, true)],
        "cg OBS still shows sp-fast until the mirror is answered"
    );
    nothing_more(&mut events).await;
    cg_shows(&bus, Some(4));
    assert_eq!(next(&mut events, 2).await, [(7, false), (4, true)]);
    nothing_more(&mut events).await;

    bus.cut(4, utc_now_100ns(), Some("sp-slow"));
    assert_eq!(next(&mut events, 1).await, [(4, true)], "the re-kick");
    nothing_more(&mut events).await;
}

/// A dashboard cut to "OBS manuál" while cg OBS shows sp-slow keeps sp-slow
/// on air (the input carries it); a manual scene cg OBS accepted takes it off.
#[tokio::test]
async fn obs_manual_keeps_on_air_what_cg_obs_shows() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(4, Some("sp-slow"));
    bus.legacy_cg().restored(4);
    let (mut events, _shutdown, _task) = authority(&bus);
    assert_eq!(next(&mut events, 1).await, [(4, true)]);

    bus.cut(PROGRAM_INPUT_ID, utc_now_100ns(), None);
    assert_eq!(next(&mut events, 1).await, [(4, true)], "still on air");
    nothing_more(&mut events).await;
    cg_shows(&bus, None);
    assert_eq!(next(&mut events, 1).await, [(4, false)]);
    nothing_more(&mut events).await;
}

/// It ends on shutdown, and when the engine's channel is gone.
#[tokio::test]
async fn the_authority_ends_on_shutdown_or_without_an_engine() {
    let bus = Arc::new(ProgramBus::new());
    let (_events, shutdown, task) = authority(&bus);
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("it ends on shutdown")
        .unwrap();

    let (events, _shutdown, task) = authority(&bus);
    drop(events);
    bus.cut(7, utc_now_100ns(), Some("sp-fast"));
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("it ends once the engine is gone")
        .unwrap();
}

// -- the engine's stale check -------------------------------------------------

/// OUT and IN, each with one normalized song.
const OUT: i64 = 7;
const IN: i64 = 8;
const SONG: i64 = 42;

async fn engine() -> PlaybackEngine {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    for (playlist_id, video_id) in [(OUT, SONG), (IN, 44)] {
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
            "INSERT INTO videos (id, playlist_id, youtube_id, normalized, file_path, \
             audio_file_path) VALUES (?, ?, ?, 1, ?, ?)",
        )
        .bind(video_id)
        .bind(playlist_id)
        .bind(format!("yt{video_id}"))
        .bind(format!("/tmp/sp-authority-{video_id}_video.mp4"))
        .bind(format!("/tmp/sp-authority-{video_id}_audio.flac"))
        .execute(&pool)
        .await
        .unwrap();
    }
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(64);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(64);
    let mut engine = PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache-authority"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    });
    engine.ensure_pipeline(OUT, "SP-7");
    engine.ensure_pipeline(IN, "SP-8");
    engine
}

/// The engine's program bus with `source` on `SP-program` (no transition
/// window: nothing is held).
fn program(engine: &PlaybackEngine, source: i64) -> Arc<ProgramBus> {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(source, None);
    assert!(engine.program.set(bus.clone()).is_ok());
    bus
}

fn on_program(engine: &PlaybackEngine, pid: i64) -> bool {
    engine.pipelines[&pid].scene_active.load(Ordering::Acquire)
}

fn state(engine: &PlaybackEngine, pid: i64) -> PlayState {
    engine.pipelines[&pid].state.clone()
}

/// An ON is applied only while the playlist is on air: it puts it on
/// program and starts a song. A stale ON (the playlist left again) is
/// dropped.
#[tokio::test]
async fn an_on_is_applied_only_while_the_playlist_is_on_air() {
    let mut engine = engine().await;
    let _bus = program(&engine, IN);
    engine
        .handle_pipeline_event(IN, PipelineEvent::OnProgram(true))
        .await;
    assert!(on_program(&engine, IN));
    assert_eq!(state(&engine, IN), PlayState::Playing { video_id: 44 });

    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(true))
        .await;
    assert!(!on_program(&engine, OUT), "a stale ON is dropped");
    assert_eq!(state(&engine, OUT), PlayState::Idle, "nothing started");
}

/// An OFF is applied only while the playlist is off air. The program's own
/// source is never taken off, and neither is a playlist cg OBS still shows
/// (its mirror unanswered): a stale OFF is dropped.
#[tokio::test]
async fn an_off_is_applied_only_while_the_playlist_is_off_air() {
    let mut engine = engine().await;
    let bus = program(&engine, OUT);
    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(true))
        .await;
    assert_eq!(state(&engine, OUT), PlayState::Playing { video_id: SONG });

    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(false))
        .await;
    assert!(on_program(&engine, OUT), "the program's source stays on");
    assert_eq!(state(&engine, OUT), PlayState::Playing { video_id: SONG });

    // IN is cut to; cg OBS still shows OUT: OUT is on air, OFF is stale.
    cg_shows(&bus, Some(OUT));
    bus.cut(IN, utc_now_100ns(), None);
    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(false))
        .await;
    assert!(on_program(&engine, OUT), "cg OBS still shows it");
    assert_eq!(state(&engine, OUT), PlayState::Playing { video_id: SONG });

    // cg OBS shows IN now: the OFF applies.
    cg_shows(&bus, Some(IN));
    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(false))
        .await;
    assert!(!on_program(&engine, OUT), "off program");
}

/// Before `start_program` sets the bus, nothing is on air.
#[tokio::test]
async fn without_a_program_bus_nothing_is_on_air() {
    let mut engine = engine().await;
    assert!(!engine.on_air_contains(OUT));
    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(true))
        .await;
    assert!(!on_program(&engine, OUT), "no bus: an ON is stale");
    let _bus = program(&engine, OUT);
    assert!(engine.on_air_contains(OUT));
    assert!(!engine.on_air_contains(IN));
}
