//! #215: the deferred scene-go-off pause. The pure [`scene_off_delay`], then
//! the engine over a real [`ProgramBus`]: the program's outgoing playlist keeps
//! playing through its transition window and pauses once the window is over,
//! every other playlist pauses at once (#221 L4b: the program's own source
//! too — the `Hold::OnProgram` settle is gone), and a scene back on program
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
fn the_pause_waits_exactly_until_the_hold_ends() {
    assert_eq!(scene_off_delay(None, T0), None, "no hold: pause now");
    assert_eq!(
        scene_off_delay(Some(Hold::Until(T0 + 333_334)), T0),
        Some(Duration::from_nanos(33_333_400)),
        "one slot of 100 ns units"
    );
    assert_eq!(
        scene_off_delay(Some(Hold::Until(T0 + 1)), T0),
        Some(Duration::from_nanos(100))
    );
    assert_eq!(
        scene_off_delay(Some(Hold::Until(T0)), T0),
        None,
        "the hold ends now"
    );
    assert_eq!(
        scene_off_delay(Some(Hold::Until(T0 - 1)), T0),
        None,
        "a hold already over"
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
    bus.select_initial(on_program, None);
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
            Ok(Some((id, PipelineEvent::SceneOffDue(_)))) if id == pid => return,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the engine's event channel closed"),
            Err(_) => panic!("no SceneOffDue for playlist {pid} within 20 s"),
        }
    }
}

const PLAYING: PlayState = PlayState::Playing { video_id: SONG };

/// The id of `pid`'s pending hold re-check (the hold marker).
fn pending_re_check(engine: &PlaybackEngine, pid: i64) -> u64 {
    engine
        .pipelines
        .get(&pid)
        .and_then(|pp| pp.scene_off_due.as_ref())
        .expect("a hold re-check is pending")
        .0
}

#[tokio::test]
async fn the_outgoing_playlist_plays_through_its_window_and_pauses_once_it_is_over() {
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let bus = program(&engine, OUT);
    // The cut lands on b(7). IN has sent no live pair yet, so the fade may
    // wait up to 15 boundaries (the #215 cue gate) and end as late as b(31):
    // OUT is held until b(32).
    bus.cut(IN, b(5), None);
    assert_eq!(bus.hold_for(OUT), Some(Hold::Until(b(32))));
    engine.set_scene_active_for_test(OUT, false); // cg OBS switched away

    // The scene leaves program 100 ms before the hold ends.
    let started = Instant::now();
    engine.scene_off_step(OUT, b(32) - 100 * MS).await;
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
    engine.scene_off_recheck(OUT, b(32) - 1).await;
    assert_eq!(state(&engine, OUT), PLAYING);
    next_scene_off_due(&mut engine, OUT).await;

    // At the hold's end it pauses exactly as a scene-go-off always did.
    engine.scene_off_recheck(OUT, b(32)).await;
    assert_eq!(state(&engine, OUT), PlayState::WaitingForScene);
    assert_eq!(paused_at(&engine, OUT), Some((SONG, 0)));
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

    // #221 L4b: the program's own source is not held either (the authority
    // never takes it off; no `Hold::OnProgram` settle any more).
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let _bus = program(&engine, OUT);
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
    let status = bus.cut(IN, utc_now_100ns() + 60 * 10_000_000, None);
    assert!(status.cut_boundary_100ns.is_some());
    engine.handle_scene_change(OUT, false).await;
    assert_eq!(state(&engine, OUT), PLAYING, "held through the window");
    // `SceneOffDue` through the engine's event handler re-checks it the same
    // (the pending re-check's own id: the event names its hold).
    let due = pending_re_check(&engine, OUT);
    engine
        .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue(due))
        .await;
    assert_eq!(state(&engine, OUT), PLAYING);
}

#[tokio::test]
async fn a_scene_back_on_program_cancels_the_pending_pause() {
    // A real hold (release 0.68.0 review round 2: a re-check with no hold
    // pending is stale, so the test no longer injects one): OUT fades out of
    // a cut to IN, held until b(32).
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let bus = program(&engine, OUT);
    bus.cut(IN, b(5), None);
    engine.set_scene_active_for_test(OUT, false);
    engine.scene_off_step(OUT, b(32) - 100 * MS).await;
    let due = pending_re_check(&engine, OUT);
    // The scene is back on program when the re-check comes: nothing happens.
    engine.handle_scene_change(OUT, true).await;
    engine
        .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue(due))
        .await;
    assert_eq!(state(&engine, OUT), PLAYING);
    assert_eq!(paused_at(&engine, OUT), None);
    // Off program again once the window is over: it pauses at once.
    engine.set_scene_active_for_test(OUT, false);
    engine.scene_off_step(OUT, b(32)).await;
    assert_eq!(state(&engine, OUT), PlayState::WaitingForScene);
    // A re-check for a pipeline that is gone is ignored.
    engine
        .handle_pipeline_event(99, PipelineEvent::SceneOffDue(due))
        .await;
}

/// Review round 2 (#221 L4b): a hold's re-check never pauses a playlist that
/// is back ON AIR (pressed again) before the playback authority's ON reached
/// the engine. That ON is queued behind the re-check (the authority's diffed
/// set has OUT), and its scene-on ends the hold. Before L4b,
/// `Hold::OnProgram` held the selected source through the settle; with it
/// deleted, the re-check paused OUT and the ON then started a new song.
#[tokio::test]
async fn a_re_check_never_pauses_a_playlist_that_is_back_on_air() {
    let mut engine = rig().await;
    playing(&mut engine, OUT);
    let bus = program(&engine, OUT);
    bus.cut(IN, b(5), None);
    engine.set_scene_active_for_test(OUT, false); // the authority's OFF
    engine.scene_off_step(OUT, b(32) - 100 * MS).await;
    let due = pending_re_check(&engine, OUT);
    // OUT pressed again: the cut back cancels the window, and the authority
    // diffed OUT on air; its ON is queued, not handled yet.
    bus.cut(OUT, b(5), None);
    assert_eq!(bus.hold_for(OUT), None, "no window holds OUT any more");
    engine.on_air.replace([OUT].into());
    engine
        .handle_pipeline_event(OUT, PipelineEvent::SceneOffDue(due))
        .await;
    assert_eq!(state(&engine, OUT), PLAYING, "not paused");
    assert_eq!(paused_at(&engine, OUT), None);
}
