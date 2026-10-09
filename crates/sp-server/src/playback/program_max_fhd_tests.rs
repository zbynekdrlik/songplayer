//! #239: the `SP-program` Spout sender's switches and telemetry
//! (`program_max_fhd.rs`): the enable rule (MAX off ⇒ FHD off with reason
//! `max_off`), the setting, the records and the `max.fhd` block.
//! Wired via `#[cfg(test)] #[path = "program_max_fhd_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sp_gpu::{ComposeStats, SpoutSendStats};
use tokio::sync::broadcast;

use super::{FHD_OFF_MAX, FHD_OFF_SETTING, fhd_off_reason, load_fhd_enabled};
use crate::playback::program_max::{MAX_NOT_RUNNING, MaxOut, run_max_settings_task};

fn compose(upload_us: u64, draw_us: u64) -> ComposeStats {
    ComposeStats {
        upload_us,
        draw_us,
        uploads: 1,
    }
}

/// MAX on, the FHD setting on.
fn both_on() -> MaxOut {
    let max = MaxOut::new();
    max.set_enabled(true);
    max.set_fhd_enabled(true);
    max
}

#[test]
fn the_fhd_sender_runs_only_with_max_and_its_own_setting_on() {
    assert_eq!(FHD_OFF_MAX, "max_off");
    assert_eq!(FHD_OFF_SETTING, "setting_off");
    // (its setting, MAX) → why it is off.
    let table = [
        (true, true, None),
        (true, false, Some("max_off")),
        (false, false, Some("max_off")),
        (false, true, Some("setting_off")),
    ];
    for (fhd, max, want) in table {
        assert_eq!(fhd_off_reason(fhd, max), want, "fhd {fhd}, max {max}");
    }
}

#[test]
fn a_new_max_has_the_fhd_sender_off_because_max_is_off() {
    let max = MaxOut::new();
    assert!(!max.fhd_enabled(), "off until the setting is applied");
    assert!(!max.fhd_wanted());
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.enabled, fhd.state.as_str(), fhd.reason),
        (false, "off", Some(FHD_OFF_MAX))
    );
    assert_eq!(fhd.spout_name, "SP-program");
    assert_eq!((fhd.listed_width, fhd.listed_height), (0, 0));
    assert_eq!(
        [
            fhd.submitted,
            fhd.failed,
            fhd.sender_backoffs,
            fhd.draw_us_p99,
            fhd.send_us_p99
        ],
        [0; 5]
    );
}

#[test]
fn max_off_turns_the_fhd_sender_off_whatever_its_setting() {
    let max = MaxOut::new();
    assert!(max.set_fhd_enabled(true), "off → on is a change");
    assert!(!max.set_fhd_enabled(true), "on → on is not");
    assert!(max.fhd_enabled());
    assert!(!max.fhd_wanted(), "MAX is still off");
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.enabled, fhd.state.as_str(), fhd.reason),
        (true, "off", Some(FHD_OFF_MAX)),
        "MAX off ⇒ FHD off, and the telemetry says why"
    );

    max.set_enabled(true);
    assert!(max.fhd_wanted());
    let _consumer = max.attach();
    let fhd = max.status().fhd;
    assert_eq!((fhd.state.as_str(), fhd.reason), ("running", None));

    assert!(max.set_fhd_enabled(false), "on → off is a change");
    assert!(!max.fhd_wanted());
    let fhd = max.status().fhd;
    assert_eq!(
        (fhd.enabled, fhd.state.as_str(), fhd.reason),
        (false, "off", Some(FHD_OFF_SETTING))
    );
    assert!(max.status().enabled, "MAX stays on");
}

#[test]
fn the_fhd_records_count_and_name_its_own_state() {
    let max = both_on();
    let _consumer = max.attach();
    max.record_fhd_sent(compose(11, 22), SpoutSendStats { send_us: 33 });
    let fhd = max.status().fhd;
    assert_eq!((fhd.state.as_str(), fhd.submitted), ("running", 1));
    assert_eq!(
        (fhd.draw_us_p99, fhd.send_us_p99),
        (22, 33),
        "the draw and the send, never the upload"
    );

    max.record_fhd_failed("why");
    max.record_fhd_skipped();
    max.record_fhd_sender_backoff();
    let fhd = max.status().fhd;
    assert_eq!(fhd.state, "error: why", "a skip keeps the failure's text");
    assert_eq!((fhd.failed, fhd.sender_backoffs), (2, 1));
    let status = max.status();
    assert_eq!(
        (status.state.as_str(), status.submitted, status.failed),
        ("running", 0, 0),
        "MAX's own counts are untouched"
    );

    max.record_fhd_sent(compose(1, 2), SpoutSendStats { send_us: 3 });
    let fhd = max.status().fhd;
    assert_eq!((fhd.state.as_str(), fhd.submitted), ("running", 2));
}

#[test]
fn the_fhd_p99s_cover_the_last_900_frames() {
    let max = MaxOut::new();
    for us in 1..=1000 {
        max.record_fhd_sent(compose(5 * us, 2 * us), SpoutSendStats { send_us: 3 * us });
    }
    let fhd = max.status().fhd;
    assert_eq!((fhd.draw_us_p99, fhd.send_us_p99), (1982, 2973));
}

#[test]
fn the_listed_size_is_what_was_read_last() {
    let max = both_on();
    assert_eq!(max.fhd_listed(), None);
    max.record_fhd_listed(Some((1920, 1080)));
    assert_eq!(max.fhd_listed(), Some((1920, 1080)));
    let fhd = max.status().fhd;
    assert_eq!((fhd.listed_width, fhd.listed_height), (1920, 1080));
    max.record_fhd_listed(None);
    assert_eq!(max.fhd_listed(), None);
    let fhd = max.status().fhd;
    assert_eq!((fhd.listed_width, fhd.listed_height), (0, 0));
}

#[test]
fn the_fhd_state_follows_the_thread_and_unsupported_wins() {
    let max = both_on();
    assert_eq!(
        max.status().fhd.state,
        format!("error: {MAX_NOT_RUNNING}"),
        "on, but no thread yet"
    );
    let consumer = max.attach();
    assert_eq!(max.status().fhd.state, "running");
    drop(consumer);
    assert_eq!(max.status().fhd.state, format!("error: {MAX_NOT_RUNNING}"));

    max.record_unsupported();
    assert_eq!(max.status().fhd.state, "unsupported");
    let consumer = max.attach();
    assert_eq!(max.status().fhd.state, "unsupported", "attach keeps it");
    drop(consumer);
    assert_eq!(max.status().fhd.state, "unsupported", "so does the end");
}

#[test]
fn the_fhd_block_has_its_api_names() {
    let max = MaxOut::new();
    let json = serde_json::to_value(max.status()).unwrap();
    let mut keys: Vec<&str> = json["fhd"]
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "draw_us_p99",
            "enabled",
            "failed",
            "listed_height",
            "listed_width",
            "reason",
            "send_us_p99",
            "sender_backoffs",
            "spout_name",
            "state",
            "submitted",
        ]
    );
    assert_eq!(json["fhd"]["reason"], "max_off");
    assert_eq!(json["fhd"]["spout_name"], "SP-program");
}

async fn settings_pool() -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

/// Poll `done` on the runtime until it holds (bounded: 20 s).
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
async fn the_fhd_setting_is_on_by_default_and_off_only_for_false() {
    use crate::db::models::set_setting;
    let pool = settings_pool().await;
    assert!(load_fhd_enabled(&pool).await.unwrap(), "no setting = ON");
    set_setting(&pool, "program_spout_fhd_enabled", "false")
        .await
        .unwrap();
    assert!(!load_fhd_enabled(&pool).await.unwrap());
    set_setting(&pool, "program_spout_fhd_enabled", "true")
        .await
        .unwrap();
    assert!(load_fhd_enabled(&pool).await.unwrap());
}

#[tokio::test]
async fn the_settings_task_applies_every_fhd_change() {
    use crate::db::models::set_setting;
    let pool = settings_pool().await;
    let max = Arc::new(MaxOut::new());
    let (shutdown, _) = broadcast::channel(1);
    let task = tokio::spawn(run_max_settings_task(
        pool.clone(),
        max.clone(),
        shutdown.subscribe(),
        Duration::from_millis(5),
    ));
    eventually("the default ON applied", || max.fhd_enabled()).await;
    assert!(max.fhd_wanted(), "MAX's default ON too");
    set_setting(&pool, "program_spout_fhd_enabled", "false")
        .await
        .unwrap();
    eventually("the switch-off applied", || !max.fhd_enabled()).await;
    assert!(max.enabled(), "MAX stays on");
    set_setting(&pool, "program_spout_fhd_enabled", "true")
        .await
        .unwrap();
    eventually("the switch-on applied", || max.fhd_enabled()).await;
    set_setting(&pool, "program_max_enabled", "false")
        .await
        .unwrap();
    eventually("MAX off", || !max.enabled()).await;
    assert!(max.fhd_enabled(), "its own setting stays on");
    assert!(!max.fhd_wanted(), "but it needs MAX");
    assert_eq!(max.status().fhd.reason, Some(FHD_OFF_MAX));
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("the task ends on shutdown")
        .unwrap();
}

/// Off Windows `start_max` applies the stored FHD setting before it
/// returns, and the FHD sender reads `unsupported` like MAX.
#[cfg(not(windows))]
#[tokio::test]
async fn off_windows_start_max_applies_the_fhd_setting_and_reads_unsupported() {
    use crate::db::models::set_setting;
    use crate::playback::program_max::start_max;
    let pool = settings_pool().await;
    let (shutdown, _) = broadcast::channel(1);
    set_setting(&pool, "program_spout_fhd_enabled", "false")
        .await
        .unwrap();
    let max = Arc::new(MaxOut::new());
    max.set_fhd_enabled(true);
    start_max(pool, max.clone(), &shutdown).await;
    assert!(
        !max.fhd_enabled(),
        "the stored OFF, applied before start_max returns"
    );
    assert_eq!(max.status().fhd.state, "unsupported");
    shutdown.send(()).unwrap();
}
