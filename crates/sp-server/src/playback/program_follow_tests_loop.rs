//! #219: `FollowLoop`'s own steps, driven one snapshot at a time (no task
//! runs, so every step's effect is complete when it returns). The follow is a
//! pure consumer of the OBS client's snapshots: a changed program scene is
//! followed, an unchanged one is not, a lookup-failed one is ignored, a
//! disconnect forgets the scene, the transition comes from the snapshot, and
//! the catch-up (start / switch-on) follows the scene changed or not.
//! Wired via `#[cfg(test)] #[path = "program_follow_tests_loop.rs"] mod tests_loop;`.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::watch;

use super::tests::{cut, fade, last_cut, lookup_failed, on_program, pool, spec_of, store, with};
use super::*;
use crate::obs::ObsSnapshot;
use crate::playback::program_bus::{ProgramBus, persist_and_cut};
use crate::playback::program_transition::{SpecSource, TransitionKind};

/// A `FollowLoop` over `pool`'s stored settings and a fresh bus.
async fn follow_loop(pool: &SqlitePool) -> (FollowLoop, Arc<ProgramBus>) {
    let bus = Arc::new(ProgramBus::new());
    let settings = load_follow_settings(pool).await.unwrap();
    let task = FollowLoop::new(Follow::new(pool.clone(), bus.clone()), settings);
    (task, bus)
}

/// The operator cuts `SP-program` to `source` by hand (the dashboard's path).
async fn manual_cut(pool: &SqlitePool, bus: &ProgramBus, source: i64) {
    persist_and_cut(pool, bus, source).await.unwrap();
}

#[tokio::test]
async fn a_changed_scene_is_followed_and_an_unchanged_one_is_not() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let (mut task, bus) = follow_loop(&pool).await;
    task.on_snapshot(&on_program("sp-fast", &[7]), false).await;
    assert_eq!(bus.status().source, Some(7));
    assert_eq!(bus.status().health.cuts, 1);
    // The operator takes the program elsewhere; a snapshot that changes only
    // cg OBS's transition must not cut it back.
    manual_cut(&pool, &bus, 9).await;
    task.on_snapshot(&with(on_program("sp-fast", &[7]), fade(500)), false)
        .await;
    assert_eq!(
        (bus.status().source, bus.status().health.cuts),
        (Some(9), 2),
        "the program scene did not change: nothing is followed"
    );
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 500, 15, SpecSource::Obs),
        "but the transition is taken"
    );
    // cg OBS switches: followed, with that transition.
    task.on_snapshot(&with(on_program("sp-slow", &[8]), fade(500)), false)
        .await;
    let st = bus.status();
    assert_eq!((st.source, st.health.cuts), (Some(8), 3));
    assert_eq!(st.transition.active.map(|w| w.n_slots), Some(15));
    assert_eq!(last_cut(&bus).scene, "sp-slow");
    // The same scene with other playlists is a change too.
    task.on_snapshot(&on_program("sp-slow", &[7]), false).await;
    assert_eq!(bus.status().source, Some(7));
}

#[tokio::test]
async fn a_lookup_failed_snapshot_is_ignored_until_its_repair() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let (mut task, bus) = follow_loop(&pool).await;
    task.on_snapshot(&on_program("sp-fast", &[7]), false).await;
    manual_cut(&pool, &bus, 9).await;
    // cg OBS switched to sp-slow but its lookup failed: the client kept
    // sp-fast's playlists. Acting on them would cut the program back to 7, a
    // scene cg OBS no longer shows.
    task.on_snapshot(&lookup_failed("sp-slow", &[7]), false)
        .await;
    assert_eq!(
        (bus.status().source, bus.status().health.cuts),
        (Some(9), 2),
        "a lookup-failed snapshot cuts nothing"
    );
    assert_eq!(last_cut(&bus).scene, "sp-fast", "and is not recorded");
    // The poll's repaired lookup is the change that is followed.
    task.on_snapshot(&on_program("sp-slow", &[8]), false).await;
    assert_eq!(bus.status().source, Some(8));
    assert_eq!(last_cut(&bus).scene, "sp-slow");
    // A failure that ends on the scene already followed changes nothing: the
    // failed snapshot never replaced the seen scene.
    manual_cut(&pool, &bus, 9).await;
    task.on_snapshot(&lookup_failed("Slido", &[8]), false).await;
    task.on_snapshot(&on_program("sp-slow", &[8]), false).await;
    assert_eq!(
        (bus.status().source, bus.status().health.cuts),
        (Some(9), 4),
        "cg OBS is back on the scene the program already followed"
    );
}

#[tokio::test]
async fn a_disconnect_forgets_the_scene_so_the_reconnect_follows_it_again() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let (mut task, bus) = follow_loop(&pool).await;
    task.on_snapshot(&on_program("sp-fast", &[7]), false).await;
    manual_cut(&pool, &bus, 9).await;
    task.on_snapshot(&ObsSnapshot::default(), false).await;
    assert_eq!(bus.status().source, Some(9), "a disconnect cuts nothing");
    task.on_snapshot(&on_program("sp-fast", &[7]), false).await;
    assert_eq!(
        (bus.status().source, bus.status().health.cuts),
        (Some(7), 3),
        "the reconnect reports cg OBS's scene: it is followed again"
    );
}

#[tokio::test]
async fn the_transition_comes_from_the_snapshot_and_stays_while_unknown() {
    let pool = pool().await;
    let (mut task, bus) = follow_loop(&pool).await;
    let connected = ObsSnapshot {
        connected: true,
        ..ObsSnapshot::default()
    };
    task.on_snapshot(&connected, true).await;
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 300, 9, SpecSource::Fallback),
        "unknown: a fade of the setting's length"
    );
    task.on_snapshot(&with(connected.clone(), fade(500)), false)
        .await;
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 500, 15, SpecSource::Obs)
    );
    task.on_snapshot(&with(connected.clone(), cut()), false)
        .await;
    assert_eq!(spec_of(&bus), (TransitionKind::Cut, 0, 0, SpecSource::Obs));
    // cg OBS goes away: the last known transition stays.
    task.on_snapshot(&ObsSnapshot::default(), false).await;
    assert_eq!(spec_of(&bus), (TransitionKind::Cut, 0, 0, SpecSource::Obs));
    assert_eq!(bus.follow().obs_transition(), Some(cut()));
    // The override still wins over cg OBS.
    store(&pool, "program_transition", "fade").await;
    task.settings = load_follow_settings(&pool).await.unwrap();
    task.on_snapshot(&with(connected, fade(500)), false).await;
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 300, 9, SpecSource::Setting)
    );
    assert_eq!(bus.status().health.cuts, 0, "no scene, no cut");
}

#[tokio::test]
async fn switching_the_follow_on_catches_up_once_whether_the_scene_changed_or_not() {
    let pool = pool().await;
    let (mut task, bus) = follow_loop(&pool).await;
    let (_tx, rx) = watch::channel(with(on_program("sp-fast", &[7]), fade(500)));
    let first = rx.borrow().clone();
    task.on_snapshot(&first, true).await;
    assert_eq!(bus.status().health.cuts, 0, "the follow is off");
    // Switched on: cg OBS's scene (unchanged since) is followed, with its
    // transition.
    store(&pool, "program_follow_obs", "true").await;
    task.on_tick(&rx).await;
    let st = bus.status();
    assert_eq!((st.source, st.health.cuts), (Some(7), 1));
    assert_eq!(st.transition.active.map(|w| w.n_slots), Some(15));
    // Still on: later polls follow nothing again.
    manual_cut(&pool, &bus, 9).await;
    task.on_tick(&rx).await;
    assert_eq!(
        (bus.status().source, bus.status().health.cuts),
        (Some(9), 2),
        "a poll while following is not a catch-up"
    );
}

#[tokio::test]
async fn a_catch_up_on_an_unknown_or_failed_scene_follows_the_next_known_one() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let (mut task, bus) = follow_loop(&pool).await;
    // Seen while following: sp-fast (7); the operator moves the program.
    task.on_snapshot(&on_program("sp-fast", &[7]), false).await;
    manual_cut(&pool, &bus, 9).await;
    // A catch-up (e.g. the follow switched on) on a lookup-failed snapshot:
    // nothing to follow yet.
    task.on_snapshot(&lookup_failed("sp-slow", &[7]), true)
        .await;
    assert_eq!(bus.status().source, Some(9));
    // The repair names the scene seen before the failure: still followed,
    // the catch-up was not done yet.
    task.on_snapshot(&on_program("sp-fast", &[7]), false).await;
    assert_eq!(
        (bus.status().source, bus.status().health.cuts),
        (Some(7), 3),
        "the catch-up waits for the next known scene"
    );
    // The same for an unknown snapshot (cg OBS away at the switch-on).
    manual_cut(&pool, &bus, 9).await;
    task.on_snapshot(&ObsSnapshot::default(), true).await;
    task.on_snapshot(&on_program("sp-fast", &[7]), false).await;
    assert_eq!(bus.status().source, Some(7));
}

#[tokio::test]
async fn with_the_follow_off_nothing_is_cut() {
    let pool = pool().await;
    let (mut task, bus) = follow_loop(&pool).await;
    task.on_snapshot(&on_program("sp-fast", &[7]), true).await;
    task.on_snapshot(&on_program("sp-slow", &[8]), false).await;
    task.on_snapshot(&ObsSnapshot::default(), false).await;
    task.on_snapshot(&on_program("sp-slow", &[8]), false).await;
    let st = bus.status();
    assert_eq!((st.source, st.health.cuts), (None, 0));
    assert_eq!(
        bus.follow()
            .status(&FollowSettings::default())
            .last_follow_cut,
        None
    );
}
