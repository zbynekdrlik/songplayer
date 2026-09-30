//! #221 L4b: the playback authority — the task over a real `ProgramBus` (its
//! on-air watch and `legacy_cg`), and the engine's stale check over an
//! in-memory DB. Wired via `#[cfg(test)] #[path =
//! "program_authority_tests.rs"] mod tests;`.

use std::collections::BTreeSet;
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

/// The authority task over `bus`: its events, the set it diffed, its
/// shutdown and its handle.
struct Authority {
    events: Events,
    diffed: OnAirPlaylists,
    shutdown: broadcast::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

fn authority(bus: &Arc<ProgramBus>) -> Authority {
    let (tx, events) = mpsc::unbounded_channel();
    let (shutdown, _) = broadcast::channel(1);
    let diffed = OnAirPlaylists::default();
    let task = tokio::spawn(run_program_authority(
        Arc::clone(bus),
        tx,
        diffed.clone(),
        shutdown.subscribe(),
    ));
    Authority {
        events,
        diffed,
        shutdown,
        task,
    }
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

fn members(diffed: &OnAirPlaylists, pids: &[i64]) -> Vec<bool> {
    pids.iter().map(|&pid| diffed.contains(pid)).collect()
}

/// The first value plays the restored program; a press is ON for the new
/// playlist while cg OBS still shows the old one (both on air until the
/// mirror's OK, the old one NOT re-kicked), then OFF for the old one; a
/// press of the same scene re-kicks it. The set it diffed is written before
/// its events.
#[tokio::test]
async fn the_authority_plays_the_restored_program_and_follows_every_change() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(7, Some("sp-fast"));
    bus.legacy_cg().restored(7);
    let mut a = authority(&bus);
    assert_eq!(
        members(&a.diffed, &[7]),
        [false],
        "nothing before its first value"
    );
    assert_eq!(
        next(&mut a.events, 1).await,
        [(7, true)],
        "the restored program"
    );
    assert_eq!(members(&a.diffed, &[4, 7]), [false, true]);
    nothing_more(&mut a.events).await;

    bus.cut(4, utc_now_100ns(), Some("sp-slow"));
    assert_eq!(next(&mut a.events, 1).await, [(4, true)]);
    assert_eq!(
        members(&a.diffed, &[4, 7]),
        [true, true],
        "cg OBS still shows sp-fast until the mirror is answered"
    );
    nothing_more(&mut a.events).await;
    cg_shows(&bus, Some(4));
    assert_eq!(next(&mut a.events, 1).await, [(7, false)]);
    assert_eq!(members(&a.diffed, &[4, 7]), [true, false]);
    nothing_more(&mut a.events).await;

    bus.cut(4, utc_now_100ns(), Some("sp-slow"));
    assert_eq!(next(&mut a.events, 1).await, [(4, true)], "the re-kick");
    nothing_more(&mut a.events).await;
}

/// A dashboard cut to "OBS manuál" while cg OBS shows sp-slow keeps sp-slow
/// on air (the input carries it); a manual scene cg OBS accepted takes it off.
#[tokio::test]
async fn obs_manual_keeps_on_air_what_cg_obs_shows() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(4, Some("sp-slow"));
    bus.legacy_cg().restored(4);
    let mut a = authority(&bus);
    assert_eq!(next(&mut a.events, 1).await, [(4, true)]);

    bus.cut(PROGRAM_INPUT_ID, utc_now_100ns(), None);
    nothing_more(&mut a.events).await; // still on air, and not re-kicked
    assert_eq!(members(&a.diffed, &[4]), [true]);
    cg_shows(&bus, None);
    assert_eq!(next(&mut a.events, 1).await, [(4, false)]);
    assert_eq!(members(&a.diffed, &[4]), [false]);
    nothing_more(&mut a.events).await;
}

/// Release 0.69.0 review 🟡 2: with every set the task publishes the ONE
/// wall owner (SP-program's playlist, else the one cg OBS was told to show),
/// before that value's events; while it owns the wall, the other member of
/// a two-member set may not write it.
#[tokio::test]
async fn the_authority_publishes_the_wall_owner_with_the_set() {
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(7, Some("sp-fast"));
    bus.legacy_cg().restored(7);
    let mut a = authority(&bus);
    assert_eq!(a.diffed.owner(), None, "nothing before its first value");
    assert!(a.diffed.may_write_wall(4), "no owner restricts nothing");
    assert_eq!(next(&mut a.events, 1).await, [(7, true)]);
    assert_eq!(a.diffed.owner(), Some(7));

    // A press of sp-slow: both on air until the mirror is answered, and the
    // program's playlist owns the wall.
    bus.cut(4, utc_now_100ns(), Some("sp-slow"));
    assert_eq!(next(&mut a.events, 1).await, [(4, true)]);
    assert_eq!(members(&a.diffed, &[4, 7]), [true, true]);
    assert_eq!(a.diffed.owner(), Some(4));
    assert!(a.diffed.may_write_wall(4));
    assert!(!a.diffed.may_write_wall(7), "7 is on air, 4 owns the wall");

    // A dashboard cut to "OBS manuál": the input carries cg OBS's sp-fast,
    // which owns the wall again.
    bus.cut(PROGRAM_INPUT_ID, utc_now_100ns(), None);
    assert_eq!(next(&mut a.events, 1).await, [(4, false)]);
    assert_eq!(a.diffed.owner(), Some(7));
    assert!(a.diffed.may_write_wall(7));
    assert!(!a.diffed.may_write_wall(4));

    // cg OBS shows a manual scene: nothing on air, no owner.
    cg_shows(&bus, None);
    assert_eq!(next(&mut a.events, 1).await, [(7, false)]);
    assert_eq!(a.diffed.owner(), None);
    nothing_more(&mut a.events).await;
}

/// It ends on shutdown, and when the engine's channel is gone.
#[tokio::test]
async fn the_authority_ends_on_shutdown_or_without_an_engine() {
    let bus = Arc::new(ProgramBus::new());
    let a = authority(&bus);
    a.shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), a.task)
        .await
        .expect("it ends on shutdown")
        .unwrap();

    let a = authority(&bus);
    drop(a.events);
    bus.cut(7, utc_now_100ns(), Some("sp-fast"));
    tokio::time::timeout(Duration::from_secs(10), a.task)
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

/// The authority task diffed `pids` as on air (the set it writes before it
/// sends that value's events).
fn diffed(engine: &PlaybackEngine, pids: &[i64]) {
    engine
        .on_air
        .replace(pids.iter().copied().collect::<BTreeSet<i64>>());
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
    diffed(&engine, &[IN]);
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
    diffed(&engine, &[OUT]);
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
    bus.cut(IN, utc_now_100ns(), None);
    diffed(&engine, &[IN, OUT]);
    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(false))
        .await;
    assert!(on_program(&engine, OUT), "cg OBS still shows it");
    assert_eq!(state(&engine, OUT), PlayState::Playing { video_id: SONG });

    // cg OBS shows IN now: the OFF applies.
    diffed(&engine, &[IN]);
    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(false))
        .await;
    assert!(!on_program(&engine, OUT), "off program");
}

/// Review round 1: the stale check reads the set the authority DIFFED, never
/// the live bus. The bus can change and change back between two task wakes;
/// the task then sends no newer event, so an event the live bus made stale
/// would be lost for good.
#[tokio::test]
async fn the_stale_check_reads_the_set_the_authority_diffed_not_the_live_bus() {
    let mut engine = engine().await;
    let _bus = program(&engine, OUT); // the live bus: OUT on air
    diffed(&engine, &[IN]); // the task has not diffed that value yet
    engine
        .handle_pipeline_event(IN, PipelineEvent::OnProgram(true))
        .await;
    assert!(on_program(&engine, IN), "IN is in the diffed set");
    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(true))
        .await;
    assert!(
        !on_program(&engine, OUT),
        "OUT's ON comes when the task diffs the bus's value"
    );
}

/// Before the authority's first value, nothing is on air.
#[tokio::test]
async fn before_the_authority_s_first_value_nothing_is_on_air() {
    let mut engine = engine().await;
    assert!(!engine.on_air_contains(OUT));
    engine
        .handle_pipeline_event(OUT, PipelineEvent::OnProgram(true))
        .await;
    assert!(!on_program(&engine, OUT), "an ON is stale");
    diffed(&engine, &[OUT]);
    assert!(engine.on_air_contains(OUT));
    assert!(!engine.on_air_contains(IN));
}

/// Review round 1: an ON for a playlist with NO pipeline (the #196 startup
/// senders ran out of their budget) creates it lazily, and it goes on
/// program.
#[tokio::test]
async fn an_on_for_a_playlist_with_no_pipeline_creates_it_on_program() {
    let mut engine = engine().await;
    engine.remove_pipeline(IN);
    diffed(&engine, &[IN]);
    engine
        .handle_pipeline_event(IN, PipelineEvent::OnProgram(true))
        .await;
    assert!(engine.pipelines.contains_key(&IN), "created");
    assert!(on_program(&engine, IN));
    assert_eq!(state(&engine, IN), PlayState::Playing { video_id: 44 });
}

/// An ON for a pipeline already on program (the re-kick) starts it again
/// when it waits, e.g. after its song ended in Single mode: SceneOn →
/// SelectAndPlay, never the lazy-create path.
#[tokio::test]
async fn an_on_re_kicks_a_waiting_pipeline_already_on_program() {
    let mut engine = engine().await;
    engine.set_scene_active_for_test(IN, true);
    engine.set_state_for_test(IN, PlayState::WaitingForScene);
    diffed(&engine, &[IN]);
    engine
        .handle_pipeline_event(IN, PipelineEvent::OnProgram(true))
        .await;
    assert_eq!(state(&engine, IN), PlayState::Playing { video_id: 44 });
}

/// Review round 1 (F1), the task and the engine together: a playlist the
/// operator PAUSED on the dashboard stays paused — its resume point kept, no
/// new song — through a dashboard cut to "OBS manuál" (cg OBS still shows
/// it), a Companion manual press (cg OBS accepts before the cut), and a
/// press of another playlist (cg OBS shows it until the mirror's OK).
#[tokio::test]
async fn a_paused_playlist_stays_paused_through_cuts_that_did_not_press_it() {
    let mut engine = engine().await;
    let bus = program(&engine, OUT);
    bus.legacy_cg().restored(OUT);
    let (shutdown, _) = broadcast::channel(1);
    let _task = tokio::spawn(run_program_authority(
        Arc::clone(&bus),
        engine.event_tx.clone(),
        engine.on_air.clone(),
        shutdown.subscribe(),
    ));
    pump(&mut engine).await;
    assert_eq!(state(&engine, OUT), PlayState::Playing { video_id: SONG });
    let pause = |engine: &mut PlaybackEngine| {
        engine.set_state_for_test(OUT, PlayState::WaitingForScene);
        engine.pipelines.get_mut(&OUT).unwrap().paused_at = Some((SONG, 60_000));
    };
    let resume_point = |engine: &PlaybackEngine| engine.pipelines[&OUT].paused_at;

    // (a) A dashboard cut to "OBS manuál"; cg OBS still shows OUT.
    pause(&mut engine);
    bus.cut(PROGRAM_INPUT_ID, utc_now_100ns(), None);
    pump(&mut engine).await;
    assert_eq!(resume_point(&engine), Some((SONG, 60_000)), "(a) kept");
    assert_eq!(state(&engine, OUT), PlayState::WaitingForScene);

    // (b) A manual press: cg OBS accepts first (OUT stays SP-program's
    // source a moment), then the cut to "OBS manuál".
    bus.cut(OUT, utc_now_100ns(), Some("sp-7"));
    pump(&mut engine).await; // the press of OUT itself re-kicks it
    pause(&mut engine);
    cg_shows(&bus, None);
    pump(&mut engine).await;
    assert_eq!(resume_point(&engine), Some((SONG, 60_000)), "(b) kept");
    bus.cut(PROGRAM_INPUT_ID, utc_now_100ns(), Some("Slido"));
    pump(&mut engine).await;
    assert_eq!(resume_point(&engine), Some((SONG, 60_000)), "(b) kept");

    // (c) A press of IN while cg OBS still shows OUT, then cg OBS's OK.
    bus.cut(OUT, utc_now_100ns(), Some("sp-7"));
    cg_shows(&bus, Some(OUT));
    pump(&mut engine).await;
    pause(&mut engine);
    bus.cut(IN, utc_now_100ns(), Some("sp-8"));
    pump(&mut engine).await;
    assert_eq!(resume_point(&engine), Some((SONG, 60_000)), "(c) kept");
    assert_eq!(state(&engine, IN), PlayState::Playing { video_id: 44 });
    cg_shows(&bus, Some(IN));
    pump(&mut engine).await;
    assert_eq!(resume_point(&engine), Some((SONG, 60_000)), "(c) kept");
    assert!(!on_program(&engine, OUT), "OUT left program");
}

/// Let the authority task run, then apply every `OnProgram` it queued (the
/// engine loop's job); the stub pipelines' own events are skipped.
async fn pump(engine: &mut PlaybackEngine) {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    while let Ok((pid, event)) = engine.event_rx.try_recv() {
        if let PipelineEvent::OnProgram(_) = event {
            engine.handle_pipeline_event(pid, event).await;
        }
    }
}
