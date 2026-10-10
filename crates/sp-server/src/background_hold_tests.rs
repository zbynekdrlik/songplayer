//! #230: the background hold — its live read, the jobs it notes, and the
//! watcher over a real `ProgramBus` + an in-memory DB. Wired via
//! `#[cfg(test)] #[path = "background_hold_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_core::background_hold::{HOLD_FOR_S, SETTING_BACKGROUND_HOLD_UNTIL};
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use super::*;
use crate::playback::program_bus::ProgramBus;
use crate::playback::wallclock::utc_now_100ns;

/// The tests that read or clear the process-wide held-job set (the
/// watcher's arm / release clear it) run one at a time. Other modules' gate tests may add jobs meanwhile, so these tests
/// assert only `Job::YtdlpUpdate`, which no other test notes.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn pool() -> SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

async fn store(pool: &SqlitePool, until_ms: i64) {
    crate::db::models::set_setting(pool, SETTING_BACKGROUND_HOLD_UNTIL, &until_ms.to_string())
        .await
        .unwrap();
}

async fn stored(pool: &SqlitePool) -> Option<String> {
    crate::db::models::get_setting(pool, SETTING_BACKGROUND_HOLD_UNTIL)
        .await
        .unwrap()
}

fn noted(job: Job) -> bool {
    held_jobs().contains(&job)
}

/// The stored end once `done` holds for it, polled every 10 ms for 5 s
/// (real time).
async fn wait_for(
    pool: &SqlitePool,
    what: &str,
    done: impl Fn(Option<i64>) -> bool,
) -> Option<i64> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let until = stored_until(pool).await;
        if done(until) {
            return until;
        }
        assert!(
            Instant::now() < deadline,
            "{what} within 5 s (stored: {until:?})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn time_left_counts_to_the_end_and_none_once_it_passed() {
    assert_eq!(time_left(Some(10_000), 0), Some(Duration::from_secs(10)));
    assert_eq!(
        time_left(Some(10_000), 9_999),
        Some(Duration::from_millis(1))
    );
    assert_eq!(time_left(Some(10_000), 10_000), None);
    assert_eq!(time_left(Some(10_000), 12_000), None);
    assert_eq!(time_left(None, 0), None);
}

#[test]
fn every_job_has_its_stable_name() {
    let names = [
        (Job::Sync, "sync"),
        (Job::Download, "download"),
        (Job::Lyrics, "lyrics"),
        (Job::Stems, "stems"),
        (Job::Dub, "dub"),
        (Job::Metadata, "metadata"),
        (Job::Peer, "peer"),
        (Job::YtdlpUpdate, "ytdlp_update"),
    ];
    for (job, name) in names {
        assert_eq!(job.as_str(), name);
    }
}

#[tokio::test]
async fn nothing_is_held_with_no_end_a_past_end_or_a_mangled_one() {
    let _g = SERIAL.lock().await;
    let pool = pool().await;
    assert!(!held(&pool).await);
    assert!(!holds(&pool, Job::YtdlpUpdate).await);
    store(&pool, now_ms() - 1_000).await;
    assert!(!held(&pool).await);
    assert!(!holds(&pool, Job::YtdlpUpdate).await);
    crate::db::models::set_setting(&pool, SETTING_BACKGROUND_HOLD_UNTIL, "soon")
        .await
        .unwrap();
    assert!(!held(&pool).await);
    assert!(!noted(Job::YtdlpUpdate), "a job that ran is not noted");
    let s = status(&pool).await;
    assert!(!s.held);
    assert_eq!(s.until_utc_ms, None);
    assert_eq!(s.remaining_s, 0);
    assert!(s.held_jobs.is_empty());
}

#[tokio::test]
async fn a_future_end_holds_and_the_held_job_shows_on_the_status() {
    let _g = SERIAL.lock().await;
    let pool = pool().await;
    let until = now_ms() + 60_000;
    store(&pool, until).await;
    assert!(held(&pool).await);
    assert!(holds(&pool, Job::YtdlpUpdate).await);
    assert!(holds(&pool, Job::YtdlpUpdate).await, "still held");
    assert!(noted(Job::YtdlpUpdate));
    let s = status(&pool).await;
    assert!(s.held);
    assert_eq!(s.until_utc_ms, Some(until));
    assert!((59..=60).contains(&s.remaining_s), "{}", s.remaining_s);
    assert_eq!(s.hold_scene, "sp-90s");
    assert_eq!(s.release_scene, "sp-slow");
    assert!(
        s.held_jobs.contains(&"ytdlp_update".to_string()),
        "{:?}",
        s.held_jobs
    );

    // Past its end it holds nothing and the status lists no job.
    store(&pool, now_ms() - 1).await;
    let s = status(&pool).await;
    assert!(!s.held);
    assert!(
        s.until_utc_ms.is_some(),
        "a stored end is shown, past or not"
    );
    assert!(s.held_jobs.is_empty());
}

#[tokio::test]
async fn arming_stores_the_end_and_forgets_the_held_jobs() {
    let _g = SERIAL.lock().await;
    let pool = pool().await;
    held_jobs().insert(Job::YtdlpUpdate);
    let until = arm(&pool, 1_000, Duration::from_secs(HOLD_FOR_S)).await;
    assert_eq!(until, Some(14_401_000));
    assert_eq!(stored(&pool).await.as_deref(), Some("14401000"));
    assert!(!noted(Job::YtdlpUpdate));
}

#[tokio::test]
async fn releasing_deletes_the_end_and_forgets_the_held_jobs() {
    let _g = SERIAL.lock().await;
    let pool = pool().await;
    store(&pool, now_ms() + 60_000).await;
    assert!(holds(&pool, Job::YtdlpUpdate).await);
    release(&pool, "test").await;
    assert_eq!(stored(&pool).await, None);
    assert!(!held(&pool).await);
    assert!(!noted(Job::YtdlpUpdate));
}

/// The watcher over `bus`: its shutdown and its handle.
fn watcher(
    pool: &SqlitePool,
    bus: &Arc<ProgramBus>,
    hold_for: Duration,
) -> (broadcast::Sender<()>, tokio::task::JoinHandle<()>) {
    let (shutdown, _) = broadcast::channel(1);
    let task = tokio::spawn(run(
        pool.clone(),
        bus.on_air(),
        hold_for,
        shutdown.subscribe(),
    ));
    (shutdown, task)
}

/// A press of sp-90s holds for 4 h from now and a re-press re-arms it;
/// another scene changes nothing; sp-slow ends it; the startup's restored
/// source presses nothing; shutdown ends the watcher.
#[tokio::test]
async fn the_watcher_arms_on_sp_90s_re_arms_and_releases_on_sp_slow() {
    let _g = SERIAL.lock().await;
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    bus.select_initial(5, Some("sp-90s"));
    let (shutdown, task) = watcher(&pool, &bus, Duration::from_secs(HOLD_FOR_S));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(stored(&pool).await, None, "the restored source is no press");

    let before = now_ms();
    bus.cut(5, utc_now_100ns(), Some("sp-90s"));
    let first = wait_for(&pool, "the hold", |u| u.is_some()).await.unwrap();
    let after = now_ms();
    let four_h = 14_400_000;
    assert!(
        (before + four_h..=after + four_h).contains(&first),
        "{first} in [{}, {}]",
        before + four_h,
        after + four_h
    );

    tokio::time::sleep(Duration::from_millis(20)).await;
    bus.cut(5, utc_now_100ns(), Some("SP-90s"));
    let again = wait_for(&pool, "the re-armed hold", |u| u.is_some_and(|u| u > first))
        .await
        .unwrap();
    assert!(again > first);

    bus.cut(7, utc_now_100ns(), Some("sp-fast"));
    bus.cut(-2, utc_now_100ns(), Some("Blank"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        stored_until(&pool).await,
        Some(again),
        "another scene changes nothing"
    );

    bus.cut(4, utc_now_100ns(), Some("sp-slow"));
    wait_for(&pool, "the release", |u| u.is_none()).await;
    assert!(!held(&pool).await);

    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the watcher ends at shutdown")
        .unwrap();
}

/// With no release, the hold ends at its end instant: the end is deleted.
#[tokio::test]
async fn the_hold_ends_by_itself_when_its_time_passes() {
    let _g = SERIAL.lock().await;
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    let (_shutdown, _task) = watcher(&pool, &bus, Duration::from_millis(2_000));
    bus.cut(5, utc_now_100ns(), Some("sp-90s"));
    wait_for(&pool, "the hold", |u| u.is_some()).await;
    assert!(held(&pool).await);
    wait_for(&pool, "the end of the hold", |u| u.is_none()).await;
    assert!(!held(&pool).await);
}

/// An end already stored when the watcher starts (a restart during the
/// service) still ends it at its time.
#[tokio::test]
async fn a_stored_end_from_before_a_restart_still_ends_the_hold() {
    let _g = SERIAL.lock().await;
    let pool = pool().await;
    store(&pool, now_ms() + 2_000).await;
    let bus = Arc::new(ProgramBus::new());
    let (_shutdown, _task) = watcher(&pool, &bus, Duration::from_secs(HOLD_FOR_S));
    assert!(held(&pool).await);
    wait_for(&pool, "the end of the restored hold", |u| u.is_none()).await;
}

/// sp-slow with nothing held stores nothing and keeps the watcher alive.
#[tokio::test]
async fn sp_slow_with_nothing_held_changes_nothing() {
    let _g = SERIAL.lock().await;
    let pool = pool().await;
    let bus = Arc::new(ProgramBus::new());
    let (_shutdown, task) = watcher(&pool, &bus, Duration::from_secs(HOLD_FOR_S));
    bus.cut(4, utc_now_100ns(), Some("sp-slow"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(stored(&pool).await, None);
    assert!(!task.is_finished());
    bus.cut(5, utc_now_100ns(), Some("sp-90s"));
    wait_for(&pool, "a later hold", |u| u.is_some()).await;
}
