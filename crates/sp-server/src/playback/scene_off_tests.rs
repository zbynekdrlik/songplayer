//! #215: the deferred scene-go-off pause. The pure [`scene_off_delay`], then
//! the engine over a real [`ProgramBus`]: the program's outgoing playlist keeps
//! playing through its transition window and pauses once the window is over,
//! the on-program playlist waits one [`CUT_SETTLE`] for the cut that follows
//! cg OBS, every other playlist pauses at once, and a scene back on program
//! cancels the pending pause. The bus runs on fixed grid stamps and the engine
//! steps are driven at chosen instants, so no assertion depends on the wall
//! clock; the real re-check timer is only ever bounded from below (a sleep
//! never ends early) and every wait is bounded (20 s).
//! Wired via `#[cfg(test)] #[path = "scene_off_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_core::ws::ServerMsg;
use tokio::sync::{broadcast, mpsc};

use super::*;
use crate::playback::pipeline::PipelineEvent;
use crate::playback::program_bus::{Hold, ProgramBus};
use crate::playback::program_transition::{SpecSource, TransitionSpec};
use crate::playback::state::PlayState;
use crate::playback::wallclock::utc_now_100ns;
use crate::playback::{PlaybackEngine, PlaybackEngineConfig};

/// 2026-09 in 100 ns since the epoch — exactly on a second (slot 0).
const T0: i64 = 17_900_000_000_000_000;
const MS: i64 = 10_000;
/// The playlist whose OBS scene leaves program, and the one cut to.
const OUT: i64 = 7;
const IN: i64 = 8;
const SONG: i64 = 42;

/// The k-th grid boundary after `floor(T0)`.
fn b(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

#[test]
fn the_pause_waits_exactly_until_the_hold_ends_and_settles_only_once() {
    assert_eq!(CUT_SETTLE, Duration::from_millis(500));
    assert_eq!(scene_off_delay(None, T0, false), None, "no hold: pause now");
    assert_eq!(scene_off_delay(None, T0, true), None);
    assert_eq!(
        scene_off_delay(Some(Hold::Until(T0 + 333_334)), T0, false),
        Some(Duration::from_nanos(33_333_400)),
        "one slot of 100 ns units"
    );
    assert_eq!(
        scene_off_delay(Some(Hold::Until(T0 + 1)), T0, true),
        Some(Duration::from_nanos(100)),
        "settled or not, a window is waited out"
    );
    assert_eq!(
        scene_off_delay(Some(Hold::Until(T0)), T0, false),
        None,
        "the hold ends now"
    );
    assert_eq!(
        scene_off_delay(Some(Hold::Until(T0 - 1)), T0, false),
        None,
        "a hold already over"
    );
    assert_eq!(
        scene_off_delay(Some(Hold::OnProgram), T0, false),
        Some(CUT_SETTLE)
    );
    assert_eq!(
        scene_off_delay(Some(Hold::OnProgram), T0, true),
        None,
        "the settle is given once, then the playlist pauses"
    );
}

async fn rig() -> PlaybackEngine {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    let (obs_tx, _) = broadcast::channel(16);
    let (resolume_tx, _) = mpsc::channel(16);
    let (ws_tx, _) = broadcast::channel::<ServerMsg>(64);
    PlaybackEngine::new(PlaybackEngineConfig {
        pool,
        cache_dir: std::path::PathBuf::from("/tmp/test-cache"),
        obs_event_tx: obs_tx,
        obs_cmd_tx: None,
        resolume_tx,
        ws_event_tx: ws_tx,
        presenter_client: None,
        ndi_health_registry: Arc::new(crate::playback::ndi_health::NdiHealthRegistry::new()),
    })
}

/// Playlist `pid` plays song 42 with its OBS scene on program.
fn playing(engine: &mut PlaybackEngine, pid: i64) {
    engine.ensure_pipeline(pid, &format!("SP-{pid}"));
    engine.set_state_for_test(pid, PlayState::Playing { video_id: SONG });
    let pp = engine.pipelines.get_mut(&pid).expect("the pipeline");
    pp.current_video_id = Some(SONG);
    pp.scene_active.store(true, Ordering::Release);
}

fn state(engine: &PlaybackEngine, pid: i64) -> PlayState {
    engine
        .pipelines
        .get(&pid)
        .expect("the pipeline")
        .state
        .clone()
}

fn paused_at(engine: &PlaybackEngine, pid: i64) -> Option<(i64, u64)> {
    engine.pipelines.get(&pid).expect("the pipeline").paused_at
}

/// A bus with `on_program` selected and the 300 ms fade (9 slots) in force,
/// handed to the engine the way `start_program` does.
fn program(engine: &PlaybackEngine, on_program: i64) -> Arc<ProgramBus> {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(on_program);
    assert!(bus.set_transition(TransitionSpec::fade(300, SpecSource::Obs)));
    assert!(engine.program.set(bus.clone()).is_ok());
    bus
}

/// Wait (at most 20 s) for the engine's own `SceneOffDue` re-check of `pid`;
/// anything else on the channel (the stub pipeline threads' events) is skipped.
async fn next_scene_off_due(engine: &mut PlaybackEngine, pid: i64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::time::timeout_at(deadline, engine.event_rx.recv()).await {
            Ok(Some((id, PipelineEvent::SceneOffDue))) if id == pid => return,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the engine's event channel closed"),
            Err(_) => panic!("no SceneOffDue for playlist {pid} within 20 s"),
        }
    }
}

const PLAYING: PlayState = PlayState::Playing { video_id: SONG };

#[tokio::test]
async fn the_outgoing_playlist_plays_through_its_window_and_pauses_once_it_is_over() {
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let bus = program(&engine, OUT);
    bus.cut(IN, b(5)); // the window b(7)..=b(15): OUT is held until b(17)
    assert_eq!(bus.hold_for(OUT), Some(Hold::Until(b(17))));
    engine.set_scene_active_for_test(OUT, false); // cg OBS switched away

    // The scene leaves program 100 ms before the hold ends.
    let started = Instant::now();
    engine.scene_off_step(OUT, false, b(17) - 100 * MS).await;
    assert_eq!(
        state(&engine, OUT),
        PLAYING,
        "it keeps playing through the fade"
    );
    assert_eq!(paused_at(&engine, OUT), None);
    next_scene_off_due(&mut engine, OUT).await;
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "the re-check comes when the hold ends, never before: {:?}",
        started.elapsed()
    );

    // A re-check while the hold still runs waits again.
    engine.scene_off_recheck(OUT, b(17) - 1).await;
    assert_eq!(state(&engine, OUT), PLAYING);
    next_scene_off_due(&mut engine, OUT).await;

    // At the hold's end it pauses exactly as a scene-go-off always did.
    engine.scene_off_recheck(OUT, b(17)).await;
    assert_eq!(state(&engine, OUT), PlayState::WaitingForScene);
    assert_eq!(paused_at(&engine, OUT), Some((SONG, 0)));
}

#[tokio::test]
async fn the_on_program_playlist_settles_once_for_the_cut_that_follows_cg_obs() {
    // Nobody cuts away (the follow is off): after one settle it pauses.
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let _bus = program(&engine, OUT);
    engine.set_scene_active_for_test(OUT, false);
    let started = Instant::now();
    engine.scene_off_step(OUT, false, b(5)).await;
    assert_eq!(state(&engine, OUT), PLAYING, "still the program's source");
    next_scene_off_due(&mut engine, OUT).await;
    assert!(started.elapsed() >= CUT_SETTLE, "{:?}", started.elapsed());
    engine.scene_off_recheck(OUT, b(20)).await;
    assert_eq!(
        state(&engine, OUT),
        PlayState::WaitingForScene,
        "settled once: it pauses"
    );

    // The follow task's cut lands during the settle: OUT fades out first.
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let bus = program(&engine, OUT);
    engine.set_scene_active_for_test(OUT, false);
    engine.scene_off_step(OUT, false, b(5)).await;
    next_scene_off_due(&mut engine, OUT).await;
    bus.cut(IN, b(5)); // the window b(7)..=b(15): held until b(17)
    let started = Instant::now();
    engine.scene_off_recheck(OUT, b(17) - 50 * MS).await;
    assert_eq!(state(&engine, OUT), PLAYING, "now held through the window");
    next_scene_off_due(&mut engine, OUT).await;
    assert!(
        started.elapsed() >= Duration::from_millis(50),
        "{:?}",
        started.elapsed()
    );
    engine.scene_off_recheck(OUT, b(17)).await;
    assert_eq!(state(&engine, OUT), PlayState::WaitingForScene);
}

#[tokio::test]
async fn every_other_playlist_pauses_at_once_as_before() {
    // No program bus at all (before `start_program`): the plain pause.
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    engine.handle_scene_change(OUT, false).await;
    assert_eq!(state(&engine, OUT), PlayState::WaitingForScene);
    assert_eq!(paused_at(&engine, OUT), Some((SONG, 0)));

    // A playlist that is neither on program nor fading out.
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let _bus = program(&engine, IN);
    engine.handle_scene_change(OUT, false).await;
    assert_eq!(state(&engine, OUT), PlayState::WaitingForScene);
    assert_eq!(paused_at(&engine, OUT), Some((SONG, 0)));
}

#[tokio::test]
async fn a_scene_off_on_the_live_clock_goes_through_the_hold() {
    // The production entry points on the real clock: a window placed a minute
    // ahead of now holds OUT (its re-check is a minute away, never awaited).
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let bus = program(&engine, OUT);
    let status = bus.cut(IN, utc_now_100ns() + 60 * 10_000_000);
    assert!(status.cut_boundary_100ns.is_some());
    engine.handle_scene_change(OUT, false).await;
    assert_eq!(state(&engine, OUT), PLAYING, "held through the window");
    // `SceneOffDue` through the engine's event handler re-checks it the same.
    engine
        .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue)
        .await;
    assert_eq!(state(&engine, OUT), PLAYING);
}

#[tokio::test]
async fn a_scene_back_on_program_cancels_the_pending_pause() {
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let _bus = program(&engine, IN); // OUT holds nothing: a re-check would pause
    // The scene is back on program when the re-check comes: nothing happens.
    engine
        .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue)
        .await;
    assert_eq!(state(&engine, OUT), PLAYING);
    assert_eq!(paused_at(&engine, OUT), None);
    // Still off program: the re-check pauses it.
    engine.set_scene_active_for_test(OUT, false);
    engine
        .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue)
        .await;
    assert_eq!(state(&engine, OUT), PlayState::WaitingForScene);
    // A re-check for a pipeline that is gone is ignored.
    engine
        .handle_pipeline_event(99, PipelineEvent::SceneOffDue)
        .await;
}
