//! #215 / #219: the follow task end to end, driven ONLY by the OBS client's
//! snapshots (a `watch` the test publishes on) and the stored settings. The
//! task asks cg OBS nothing. `FollowLoop`'s own steps run in
//! `program_follow_tests_loop.rs`. Every wait is bounded (20 s); an effect is
//! proven by a later observable one (a spec, a cut), never by a sleep.
//! Shares the helpers of `program_follow_tests.rs`.
//! Wired via `#[cfg(test)] #[path = "program_follow_tests_task.rs"] mod tests_task;`.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use super::tests::{cut, fade, last_cut, on_program, pool, spec_of, store, with};
use super::*;
use crate::obs::ObsSnapshot;
use crate::playback::program_bus::{ProgramBus, SETTING_PROGRAM_SOURCE, persist_and_cut};
use crate::playback::program_transition::{ObsTransition, SpecSource, TransitionKind};

/// A poll interval that never comes round again after the first tick.
const NO_POLL: Duration = Duration::from_secs(3600);
/// A settings poll fast enough for a test.
const POLL: Duration = Duration::from_millis(20);

/// The follow task running on snapshots the test publishes.
struct TaskRig {
    pool: SqlitePool,
    bus: Arc<ProgramBus>,
    /// The OBS client's side of the snapshot channel (`None`: no OBS client).
    obs: Option<watch::Sender<ObsSnapshot>>,
    shutdown: broadcast::Sender<()>,
    task: JoinHandle<()>,
}

/// Start the task over `pool` (settings stored first) on the snapshot
/// channel `rx`; the settings are re-read every `poll`.
fn start_on(
    pool: &SqlitePool,
    obs: Option<watch::Sender<ObsSnapshot>>,
    rx: watch::Receiver<ObsSnapshot>,
    poll: Duration,
) -> TaskRig {
    let bus = Arc::new(ProgramBus::new());
    let follow = Follow::new(pool.clone(), bus.clone());
    let (shutdown, shutdown_rx) = broadcast::channel(1);
    let task = tokio::spawn(run_follow_task(follow, rx, shutdown_rx, poll));
    TaskRig {
        pool: pool.clone(),
        bus,
        obs,
        shutdown,
        task,
    }
}

/// Start the task on a snapshot channel holding `initial`.
fn start(pool: &SqlitePool, initial: ObsSnapshot, poll: Duration) -> TaskRig {
    let (obs, rx) = watch::channel(initial);
    start_on(pool, Some(obs), rx, poll)
}

/// Connected to cg OBS, no program scene read yet, its transition `t`.
fn connected(t: ObsTransition) -> ObsSnapshot {
    ObsSnapshot {
        connected: true,
        transition: Some(t),
        ..ObsSnapshot::default()
    }
}

impl TaskRig {
    /// The OBS client publishes `snapshot`.
    fn publish(&self, snapshot: ObsSnapshot) {
        self.obs
            .as_ref()
            .expect("an OBS client")
            .send_replace(snapshot);
    }

    /// Wait (at most 20 s) until `done` holds.
    async fn until(&self, what: &str, done: impl Fn(&ProgramBus) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !done(&self.bus) {
            assert!(tokio::time::Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Wait (at most 20 s) until the bus's spec is `want`.
    async fn spec_becomes(&self, want: (TransitionKind, u32, u32, SpecSource)) {
        self.until(&format!("the spec {want:?}"), |bus| spec_of(bus) == want)
            .await;
    }

    /// Wait (at most 20 s) until `SP-program` shows `source`.
    async fn source_becomes(&self, source: i64) {
        self.until(&format!("a cut to {source}"), |bus| {
            bus.status().source == Some(source)
        })
        .await;
    }

    /// The operator cuts `SP-program` to `source` by hand.
    async fn manual_cut(&self, source: i64) {
        persist_and_cut(&self.pool, &self.bus, source)
            .await
            .unwrap();
    }

    /// Stop the task and see it end within 20 s.
    async fn stop(self) {
        self.shutdown.send(()).expect("the task listens");
        tokio::time::timeout(Duration::from_secs(20), self.task)
            .await
            .expect("the task stops on shutdown")
            .expect("the task did not panic");
    }
}

#[tokio::test]
async fn a_following_task_catches_up_to_cg_obs_program_scene_at_start() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    let rig = start(&pool, with(on_program("sp-slow", &[8]), fade(500)), POLL);
    rig.source_becomes(8).await;
    assert_eq!(last_cut(&rig.bus).scene, "sp-slow");
    assert_eq!(
        rig.bus.status().transition.active.map(|w| w.n_slots),
        Some(15),
        "the start's cut fades with cg OBS's 500 ms transition"
    );
    // While it keeps following, the settings polls follow nothing again.
    rig.manual_cut(7).await;
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    assert_eq!(rig.bus.status().source, Some(7));
    rig.stop().await;
}

#[tokio::test]
async fn switching_the_follow_on_catches_up_once() {
    let pool = pool().await;
    let rig = start(&pool, with(on_program("sp-fast", &[7]), fade(500)), POLL);
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    assert_eq!(rig.bus.status().health.cuts, 0, "no follow: no cut");
    store(&pool, "program_follow_obs", "true").await;
    rig.source_becomes(7).await;
    // Later polls, still following, cut nothing.
    rig.manual_cut(9).await;
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    assert_eq!(
        (rig.bus.status().source, rig.bus.status().health.cuts),
        (Some(9), 2)
    );
    rig.stop().await;
}

#[tokio::test]
async fn switching_the_follow_on_with_a_new_transition_cuts_with_the_new_one() {
    let pool = pool().await;
    store(&pool, "program_transition", "cut").await;
    let rig = start(&pool, with(on_program("sp-fast", &[7]), fade(500)), POLL);
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    // ONE save switches the follow on and picks a 400 ms fade.
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES \
         ('program_follow_obs', 'true'), ('program_transition', 'fade'), \
         ('program_transition_ms', '400') \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    rig.source_becomes(7).await;
    assert_eq!(
        rig.bus.status().transition.active.map(|w| w.n_slots),
        Some(12),
        "the catch-up cut fades with the 400 ms saved together with the switch"
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_scene_change_is_followed_with_cg_obs_transition() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    // cg OBS shows a manual scene; the input is off, so the start keeps.
    let rig = start(&pool, with(on_program("Slido", &[]), fade(500)), NO_POLL);
    rig.until("the start's catch-up", |bus| {
        bus.follow()
            .status(&FollowSettings::default())
            .last_follow_cut
            .is_some()
    })
    .await;
    assert_eq!(last_cut(&rig.bus).action, "keep");
    rig.publish(with(on_program("sp-fast", &[7]), fade(500)));
    rig.source_becomes(7).await;
    assert_eq!(
        crate::db::models::get_setting(&rig.pool, SETTING_PROGRAM_SOURCE)
            .await
            .unwrap()
            .as_deref(),
        Some("7")
    );
    let followed = last_cut(&rig.bus);
    assert_eq!(
        (followed.scene.as_str(), followed.action),
        ("sp-fast", "playlist")
    );
    let window = rig.bus.status().transition.active.expect("a fade window");
    assert_eq!(
        (window.from, window.to, window.n_slots),
        (None, 7, 15),
        "the cut uses cg OBS's 500 ms fade"
    );
    rig.stop().await;
}

#[tokio::test]
async fn the_task_takes_cg_obs_transition_from_each_snapshot() {
    let pool = pool().await;
    let rig = start(&pool, connected(fade(500)), NO_POLL);
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    rig.publish(connected(fade(1000)));
    rig.spec_becomes((TransitionKind::Fade, 1000, 30, SpecSource::Obs))
        .await;
    rig.publish(connected(cut()));
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Obs))
        .await;
    assert_eq!(rig.bus.follow().obs_transition(), Some(cut()));
    rig.stop().await;
}

#[tokio::test]
async fn with_the_follow_off_a_scene_change_cuts_nothing() {
    let pool = pool().await;
    let rig = start(&pool, connected(fade(500)), NO_POLL);
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    rig.publish(with(on_program("sp-fast", &[7]), fade(500)));
    // A later snapshot with a new transition: once its spec is on the bus,
    // the scene change before it has been handled.
    rig.publish(with(on_program("sp-fast", &[7]), fade(1000)));
    rig.spec_becomes((TransitionKind::Fade, 1000, 30, SpecSource::Obs))
        .await;
    let st = rig.bus.status();
    assert_eq!((st.source, st.health.cuts), (None, 0));
    assert_eq!(
        rig.bus
            .follow()
            .status(&FollowSettings::default())
            .last_follow_cut,
        None
    );
    rig.stop().await;
}

#[tokio::test]
async fn the_settings_are_reread_and_override_cg_obs_transition() {
    let pool = pool().await;
    let rig = start(&pool, connected(fade(500)), POLL);
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    store(&pool, "program_transition", "fade").await;
    store(&pool, "program_transition_ms", "400").await;
    rig.spec_becomes((TransitionKind::Fade, 400, 12, SpecSource::Setting))
        .await;
    store(&pool, "program_transition", "obs").await;
    rig.spec_becomes((TransitionKind::Fade, 500, 15, SpecSource::Obs))
        .await;
    rig.stop().await;
}

#[tokio::test]
async fn without_an_obs_client_only_the_settings_polls_run() {
    let pool = pool().await;
    store(&pool, "program_follow_obs", "true").await;
    // No OBS client: the channel is closed from the start.
    let (_, rx) = watch::channel(ObsSnapshot::default());
    let rig = start_on(&pool, None, rx, POLL);
    rig.spec_becomes((TransitionKind::Fade, 300, 9, SpecSource::Fallback))
        .await;
    // The closed channel never starves the polls.
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    store(&pool, "program_transition", "fade").await;
    rig.spec_becomes((TransitionKind::Fade, 300, 9, SpecSource::Setting))
        .await;
    assert_eq!(rig.bus.status().health.cuts, 0, "nothing to follow");
    rig.stop().await;
}
