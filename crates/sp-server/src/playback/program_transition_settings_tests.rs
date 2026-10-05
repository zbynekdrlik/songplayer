//! #221 L5: the transition-settings task (the half of the deleted OBS follow
//! task that survives): the settings, one read onto a real `ProgramBus`, and
//! the task on its poll. Every wait is bounded (20 s); an effect is proven by
//! the spec it puts on the bus, never by a sleep.
//! Wired via `#[cfg(test)] #[path = "program_transition_settings_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::broadcast;

use super::*;
use crate::playback::program_bus::ProgramBus;
use crate::playback::program_transition::{SpecSource, TransitionKind, TransitionMode};

/// A poll interval that never comes round again after the first tick.
const NO_POLL: Duration = Duration::from_secs(3600);
/// A settings poll fast enough for a test.
const POLL: Duration = Duration::from_millis(20);

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

async fn store(pool: &SqlitePool, key: &str, value: &str) {
    crate::db::models::set_setting(pool, key, value)
        .await
        .unwrap();
}

/// The spec on the bus as `(kind, duration_ms, n_slots, source)`.
fn spec_of(bus: &ProgramBus) -> (TransitionKind, u32, u32, SpecSource) {
    let t = bus.status().transition;
    (t.kind, t.duration_ms, t.n_slots, t.source)
}

#[tokio::test]
async fn the_settings_default_to_the_300_ms_fade_and_load_trimmed() {
    let pool = pool().await;
    let defaults = load_transition_settings(&pool).await.unwrap();
    assert_eq!(
        defaults,
        TransitionSettings {
            mode: None,
            ms: 300,
        }
    );
    assert_eq!(
        defaults.spec(),
        TransitionSpec::fade(300, SpecSource::Fallback),
        "none chosen: the default fade"
    );
    store(&pool, "program_transition", " fade ").await;
    store(&pool, "program_transition_ms", "450").await;
    let loaded = load_transition_settings(&pool).await.unwrap();
    assert_eq!(
        loaded,
        TransitionSettings {
            mode: Some(TransitionMode::Fade),
            ms: 450,
        }
    );
    assert_eq!(
        loaded.spec(),
        TransitionSpec::fade(450, SpecSource::Setting)
    );
    store(&pool, "program_transition", "cut").await;
    assert_eq!(
        load_transition_settings(&pool).await.unwrap().spec(),
        TransitionSpec::cut(SpecSource::Setting)
    );
    // The retired follow's `obs` is no choice any more: the default fade.
    store(&pool, "program_transition", "obs").await;
    assert_eq!(
        load_transition_settings(&pool).await.unwrap().spec(),
        TransitionSpec::fade(450, SpecSource::Fallback)
    );
    pool.close().await;
    assert!(load_transition_settings(&pool).await.is_err());
}

#[tokio::test]
async fn one_read_puts_the_spec_on_the_bus_and_an_unreadable_store_keeps_it() {
    let pool = pool().await;
    let bus = ProgramBus::new();
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Cut, 0, 0, SpecSource::Fallback),
        "the bus starts on a hard cut"
    );
    assert_eq!(
        apply_transition_settings(&pool, &bus).await,
        Some(TransitionSpec::fade(300, SpecSource::Fallback))
    );
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Fade, 300, 9, SpecSource::Fallback)
    );
    store(&pool, "program_transition", "cut").await;
    assert_eq!(
        apply_transition_settings(&pool, &bus).await,
        Some(TransitionSpec::cut(SpecSource::Setting))
    );
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Cut, 0, 0, SpecSource::Setting)
    );
    pool.close().await;
    assert_eq!(apply_transition_settings(&pool, &bus).await, None);
    assert_eq!(
        spec_of(&bus),
        (TransitionKind::Cut, 0, 0, SpecSource::Setting),
        "the transition in force stays"
    );
}

/// The task over a real bus: its handle and shutdown.
struct TaskRig {
    bus: Arc<ProgramBus>,
    shutdown: broadcast::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

fn start(pool: &SqlitePool, poll: Duration) -> TaskRig {
    let bus = Arc::new(ProgramBus::new());
    let (shutdown, rx) = broadcast::channel(1);
    let task = tokio::spawn(run_transition_settings_task(
        pool.clone(),
        Arc::clone(&bus),
        rx,
        poll,
    ));
    TaskRig {
        bus,
        shutdown,
        task,
    }
}

impl TaskRig {
    /// Wait (at most 20 s) until the bus's spec is `want`.
    async fn spec_becomes(&self, want: (TransitionKind, u32, u32, SpecSource)) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while spec_of(&self.bus) != want {
            assert!(
                tokio::time::Instant::now() < deadline,
                "never the spec {want:?} (it is {:?})",
                spec_of(&self.bus)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
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

/// The task reads the settings at once (a poll that never comes round
/// again still puts the stored spec on the bus).
#[tokio::test]
async fn the_task_applies_the_stored_spec_at_start() {
    let pool = pool().await;
    store(&pool, "program_transition", "fade").await;
    store(&pool, "program_transition_ms", "1000").await;
    let rig = start(&pool, NO_POLL);
    rig.spec_becomes((TransitionKind::Fade, 1000, 30, SpecSource::Setting))
        .await;
    rig.stop().await;
}

/// A save applies on the next poll, whichever setting it changes.
#[tokio::test]
async fn the_task_re_reads_the_settings_every_poll() {
    let pool = pool().await;
    let rig = start(&pool, POLL);
    rig.spec_becomes((TransitionKind::Fade, 300, 9, SpecSource::Fallback))
        .await;
    store(&pool, "program_transition", "cut").await;
    rig.spec_becomes((TransitionKind::Cut, 0, 0, SpecSource::Setting))
        .await;
    store(&pool, "program_transition", "fade").await;
    store(&pool, "program_transition_ms", "400").await;
    rig.spec_becomes((TransitionKind::Fade, 400, 12, SpecSource::Setting))
        .await;
    store(&pool, "program_transition", "obs").await;
    rig.spec_becomes((TransitionKind::Fade, 400, 12, SpecSource::Fallback))
        .await;
    rig.stop().await;
}
