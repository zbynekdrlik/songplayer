//! #223 S2: the `SP-program-MAX` hand-off ([`MaxOut`]): what is taken and
//! when, the coalescing, the thread's steps, the telemetry and the setting.
//! Wired via `#[cfg(test)] #[path = "program_max_tests.rs"] mod tests;`.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use sp_gpu::{ComposeStats, GpuError, SpoutSendStats};
use tokio::sync::broadcast;

use super::{
    MAX_HANDOFF_BOUND, MAX_NOT_RUNNING, MAX_SETTINGS_POLL, MaxJob, MaxNext, MaxOut, MaxPhase,
    MaxPicture, load_max_enabled, run_max_settings_task, start_max, state_label,
};
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_max_send::SendTiming;
use crate::playback::submit_handoff::SubmitJob;

/// A black job stamped `stamp`.
fn black(stamp: i64) -> MaxJob {
    MaxJob::Black { stamp_100ns: stamp }
}

/// The stamp of the ready step's job, or a panic naming what came instead.
/// The tests take steps with `try_next`, never the blocking `next`, so no
/// wrong step can hang the test binary.
fn job_stamp(step: Option<MaxNext>) -> i64 {
    match step {
        Some(MaxNext::Job(job, _)) => job.stamp_100ns(),
        other => panic!("expected a job, got {other:?}"),
    }
}

/// A MAX that takes jobs: on, a thread attached (the guard is the caller's).
fn taking(max: &MaxOut) -> super::Consumer<'_> {
    max.set_enabled(true);
    max.attach()
}

fn lost(call: &'static str) -> GpuError {
    GpuError::DeviceLost {
        call,
        hresult: 0x887A_0005,
    }
}

#[test]
fn a_new_max_is_off_and_builds_no_job() {
    let max = MaxOut::new();
    assert!(!max.enabled());
    assert!(!max.accepting());
    let offered = max.offer_with(|| panic!("no job is built while MAX takes nothing"));
    assert!(!offered);
    assert_eq!(max.queued(), 0);
    let status = max.status();
    assert_eq!(
        (status.enabled, status.state.as_str()),
        (false, "off"),
        "off until the setting is applied"
    );
    assert_eq!((status.width, status.height), (3840, 2160));
    assert_eq!(status.spout_name, "SP-program-MAX");
    assert_eq!(status.adapter, None, "no compositor built yet");
    assert_eq!(
        [
            status.submitted,
            status.coalesced,
            status.failed,
            status.device_resets,
            status.sender_backoffs
        ],
        [0; 5]
    );
}

#[test]
fn an_offer_is_taken_only_while_on_with_a_thread_and_not_stopped() {
    let max = MaxOut::new();
    assert!(max.set_enabled(true), "off → on is a change");
    assert!(!max.set_enabled(true), "on → on is not");
    assert!(!max.offer_with(|| black(1)), "on, but no thread takes jobs");
    let consumer = max.attach();
    assert!(max.accepting());
    assert!(max.offer_with(|| black(2)));
    assert_eq!(max.queued(), 1);
    max.stop();
    assert!(!max.accepting());
    assert!(!max.offer_with(|| black(3)), "stopped");
    assert_eq!(max.queued(), 1);
    drop(consumer);
}

#[test]
fn a_full_queue_drops_its_oldest_job_and_counts_it() {
    let max = MaxOut::new();
    let _consumer = taking(&max);
    for stamp in 1..=5 {
        assert!(max.offer_with(|| black(stamp)));
    }
    assert_eq!(max.queued(), MAX_HANDOFF_BOUND);
    assert_eq!(MAX_HANDOFF_BOUND, 2, "revision 2's D4: two deep");
    assert_eq!(max.status().coalesced, 3, "stamps 1, 2 and 3 were dropped");
    assert_eq!(job_stamp(max.try_next(false)), 4, "the oldest kept first");
    assert_eq!(job_stamp(max.try_next(false)), 5);
    assert_eq!(max.queued(), 0);
    assert!(max.try_next(false).is_none(), "nothing left: it would wait");
}

#[test]
fn switching_off_drops_the_queued_jobs_and_asks_a_holding_thread_to_release() {
    let max = MaxOut::new();
    let _consumer = taking(&max);
    max.offer_with(|| black(1));
    max.offer_with(|| black(2));
    assert!(max.set_enabled(false), "on → off is a change");
    assert_eq!(max.queued(), 0, "off drops what waits");
    assert!(!max.accepting());
    assert!(matches!(max.try_next(true), Some(MaxNext::Release)));
    assert!(
        max.try_next(false).is_none(),
        "off and holding nothing: it waits"
    );
    assert!(max.set_enabled(true));
    assert_eq!(max.queued(), 0, "nothing came back on");
    max.offer_with(|| black(3));
    assert_eq!(
        job_stamp(max.try_next(true)),
        3,
        "on: a holding thread gets jobs"
    );
}

#[test]
fn a_stop_wins_over_a_queued_job_and_over_a_release() {
    let max = MaxOut::new();
    let _consumer = taking(&max);
    max.offer_with(|| black(1));
    max.stop();
    assert!(matches!(max.try_next(false), Some(MaxNext::Stop)));
    max.set_enabled(false);
    assert!(matches!(max.try_next(true), Some(MaxNext::Stop)));
}

/// `next` on its own thread; the step it returned comes on the channel.
fn next_on_thread(max: &Arc<MaxOut>, holding: bool) -> mpsc::Receiver<MaxNext> {
    let (tx, rx) = mpsc::channel();
    let max = max.clone();
    std::thread::spawn(move || {
        let _ = tx.send(max.next(holding));
    });
    rx
}

#[test]
fn a_waiting_thread_wakes_for_a_job_an_off_setting_and_a_stop() {
    let max = Arc::new(MaxOut::new());
    let _consumer = taking(&max);

    let job = next_on_thread(&max, false);
    assert!(
        job.recv_timeout(Duration::from_millis(100)).is_err(),
        "nothing to do: it waits"
    );
    max.offer_with(|| black(7));
    let next = job.recv_timeout(Duration::from_secs(20)).expect("woken");
    assert_eq!(job_stamp(Some(next)), 7);

    let release = next_on_thread(&max, true);
    assert!(release.recv_timeout(Duration::from_millis(100)).is_err());
    max.set_enabled(false);
    let next = release
        .recv_timeout(Duration::from_secs(20))
        .expect("woken");
    assert!(matches!(next, MaxNext::Release), "{next:?}");

    let stop = next_on_thread(&max, false);
    assert!(stop.recv_timeout(Duration::from_millis(100)).is_err());
    max.stop();
    let next = stop.recv_timeout(Duration::from_secs(20)).expect("woken");
    assert!(matches!(next, MaxNext::Stop), "{next:?}");
}

#[test]
fn an_attached_thread_reads_running_and_its_end_not_running() {
    let max = MaxOut::new();
    max.set_enabled(true);
    assert_eq!(
        max.status().state,
        format!("error: {MAX_NOT_RUNNING}"),
        "on, but no thread yet"
    );
    let consumer = max.attach();
    assert_eq!(max.status().state, "running");
    max.offer_with(|| black(1));
    drop(consumer);
    assert_eq!(max.status().state, format!("error: {MAX_NOT_RUNNING}"));
    assert!(!max.accepting(), "nobody takes jobs any more");
    assert_eq!(max.queued(), 0, "the jobs it left are dropped");
}

#[test]
fn unsupported_wins_over_the_setting_and_the_thread() {
    let max = MaxOut::new();
    max.record_unsupported();
    assert_eq!(max.status().state, "unsupported", "off and unsupported");
    max.set_enabled(true);
    assert_eq!(max.status().state, "unsupported");
    let consumer = max.attach();
    assert_eq!(max.status().state, "unsupported", "attach keeps it");
    drop(consumer);
    assert_eq!(max.status().state, "unsupported", "so does the end");
}

#[test]
fn the_state_label_is_the_platform_then_the_setting_then_the_thread() {
    let failed = MaxPhase::Failed("why".into());
    let table = [
        (MaxPhase::Unsupported, false, "unsupported"),
        (MaxPhase::Unsupported, true, "unsupported"),
        (MaxPhase::Running, false, "off"),
        (failed.clone(), false, "off"),
        (MaxPhase::Running, true, "running"),
        (failed, true, "error: why"),
    ];
    for (phase, enabled, want) in table {
        assert_eq!(state_label(&phase, enabled), want, "{phase:?} {enabled}");
    }
}

fn compose(upload_us: u64, draw_us: u64) -> ComposeStats {
    ComposeStats {
        upload_us,
        draw_us,
        uploads: 1,
    }
}

#[test]
fn the_records_count_and_name_the_state() {
    let max = MaxOut::new();
    let _consumer = taking(&max);
    max.record_sent(compose(10, 20), SpoutSendStats { send_us: 30 });
    let status = max.status();
    assert_eq!(status.submitted, 1);
    assert_eq!(
        (status.upload_us_p99, status.draw_us_p99, status.send_us_p99),
        (10, 20, 30)
    );

    let no_adapter = GpuError::NoAdapter.to_string();
    max.record_failed(&no_adapter);
    max.record_failed(&no_adapter);
    assert_eq!(max.status().state, format!("error: {no_adapter}"));
    max.record_failed(&lost("compose").to_string());
    max.record_skipped();
    let status = max.status();
    assert_eq!(status.failed, 4, "three failures and a skip");
    assert_eq!(
        status.state,
        format!("error: {}", lost("compose")),
        "a skip keeps the failure's text"
    );

    max.record_device_reset();
    max.record_sender_backoff();
    max.record_sender_backoff();
    max.record_adapter("NVIDIA GeForce RTX 3070 Ti".into());
    let status = max.status();
    assert_eq!((status.device_resets, status.sender_backoffs), (1, 2));
    assert_eq!(
        status.adapter.as_deref(),
        Some("NVIDIA GeForce RTX 3070 Ti")
    );

    max.record_sent(compose(1, 2), SpoutSendStats { send_us: 3 });
    let status = max.status();
    assert_eq!((status.state.as_str(), status.submitted), ("running", 2));
}

#[test]
fn the_p99s_cover_the_last_900_frames() {
    let max = MaxOut::new();
    for us in 1..=1000 {
        max.record_sent(compose(us, 2 * us), SpoutSendStats { send_us: 3 * us });
    }
    // The window keeps 101..=1000; its p99 is the 891st of 900: 991.
    let status = max.status();
    assert_eq!(
        (status.upload_us_p99, status.draw_us_p99, status.send_us_p99),
        (991, 1982, 2973)
    );
}

#[test]
fn the_max_block_has_its_api_names() {
    let max = MaxOut::new();
    let json = serde_json::to_value(max.status()).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "adapter",
            "coalesced",
            "device_resets",
            "draw_us_p99",
            "enabled",
            "failed",
            "height",
            "send_at_us_max",
            "send_at_us_p50",
            "send_at_us_p99",
            "send_late",
            "send_us_p99",
            "sender_backoffs",
            "spout_name",
            "state",
            "submitted",
            "upload_us_p99",
            "width",
        ]
    );
    assert_eq!(json["width"], 3840);
    assert_eq!(json["spout_name"], "SP-program-MAX");
}

#[test]
fn a_jobs_stamp_is_its_boundary() {
    let picture = MaxPicture {
        width: 2,
        height: 2,
        stride: 2,
        video: SharedFrame::new(vec![0; 6]),
    };
    assert_eq!(black(11).stamp_100ns(), 11);
    let one = MaxJob::Picture {
        stamp_100ns: 12,
        picture: picture.clone(),
    };
    assert_eq!(one.stamp_100ns(), 12);
    let fade = MaxJob::Fade {
        stamp_100ns: 13,
        from: Some(picture),
        to: None,
        weight_q8: 128,
    };
    assert_eq!(fade.stamp_100ns(), 13);
}

#[test]
fn a_picture_is_the_jobs_own_frame_and_layout() {
    let job = SubmitJob {
        width: 6,
        height: 4,
        stride: 8,
        video: SharedFrame::new(vec![7; 48]),
        audio: Vec::new(),
        video_tc_100ns: 5,
        audio_tc_100ns: 5,
        live: true,
    };
    let picture = MaxPicture::of(&job);
    assert_eq!((picture.width, picture.height, picture.stride), (6, 4, 8));
    assert!(picture.video.ptr_eq(&job.video), "an Arc bump, no copy");
    let nv12 = picture.nv12(42);
    assert_eq!(
        (nv12.id, nv12.width, nv12.height, nv12.stride),
        (42, 6, 4, 8)
    );
    assert_eq!(nv12.data.as_ptr(), job.video.as_ptr(), "the same bytes");
    assert_eq!(nv12.data.len(), 48);
}

async fn settings_pool() -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn the_setting_is_on_by_default_and_off_only_for_false() {
    use crate::db::models::set_setting;
    let pool = settings_pool().await;
    assert!(load_max_enabled(&pool).await.unwrap(), "no setting = ON");
    set_setting(&pool, "program_max_enabled", "false")
        .await
        .unwrap();
    assert!(!load_max_enabled(&pool).await.unwrap());
    set_setting(&pool, "program_max_enabled", "true")
        .await
        .unwrap();
    assert!(load_max_enabled(&pool).await.unwrap());
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
async fn the_settings_task_applies_every_change_and_stops_max_at_shutdown() {
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
    eventually("the default ON applied", || max.enabled()).await;
    set_setting(&pool, "program_max_enabled", "false")
        .await
        .unwrap();
    eventually("the switch-off applied", || !max.enabled()).await;
    set_setting(&pool, "program_max_enabled", "true")
        .await
        .unwrap();
    eventually("the switch-on applied", || max.enabled()).await;

    let _consumer = max.attach();
    assert!(max.accepting());
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("the task ends on shutdown")
        .unwrap();
    assert!(!max.accepting(), "stopped");
    assert!(matches!(max.try_next(false), Some(MaxNext::Stop)));
    assert_eq!(MAX_SETTINGS_POLL, Duration::from_secs(5));
}

/// Off Windows `start_max` applies the stored setting before it returns,
/// starts no thread and reads `unsupported`.
#[cfg(not(windows))]
#[tokio::test]
async fn off_windows_start_max_applies_the_setting_and_reads_unsupported() {
    use crate::db::models::set_setting;
    let pool = settings_pool().await;
    let (shutdown, _) = broadcast::channel(1);

    let on = Arc::new(MaxOut::new());
    start_max(pool.clone(), on.clone(), &shutdown).await;
    assert!(
        on.enabled(),
        "the default ON, applied before start_max returns"
    );
    assert_eq!(on.status().state, "unsupported");
    assert!(!on.accepting(), "no thread takes jobs");

    set_setting(&pool, "program_max_enabled", "false")
        .await
        .unwrap();
    let off = Arc::new(MaxOut::new());
    start_max(pool, off.clone(), &shutdown).await;
    assert!(
        !off.enabled(),
        "the stored OFF, applied before start_max returns"
    );
    assert_eq!(off.status().state, "unsupported");
    shutdown.send(()).unwrap();
}

/// On Windows `start_max` starts the `program-max` thread (on the
/// production GPU; no job is offered here, so it builds nothing): it reads
/// `running`, and its end on shutdown reads not running.
#[cfg(windows)]
#[tokio::test]
async fn on_windows_start_max_starts_the_thread_and_shutdown_ends_it() {
    let pool = settings_pool().await;
    let (shutdown, _) = broadcast::channel(1);
    let max = Arc::new(MaxOut::new());
    start_max(pool, max.clone(), &shutdown).await;
    assert!(
        max.enabled(),
        "the default ON, applied before start_max returns"
    );
    eventually("the thread takes jobs", || max.accepting()).await;
    assert_eq!(max.status().state, "running");
    shutdown.send(()).unwrap();
    eventually("the thread ended", || {
        max.status().state == format!("error: {MAX_NOT_RUNNING}")
    })
    .await;
}

/// #223 follow-up: each job carries the instant the program offered it
/// (its Spout send is due `MAX_SEND_LEAD` later).
#[test]
fn an_offered_job_carries_the_instant_it_was_offered() {
    let max = MaxOut::new();
    let _consumer = taking(&max);
    let before = Instant::now();
    assert!(max.offer_with(|| black(7)));
    let after = Instant::now();
    match max.try_next(false) {
        Some(MaxNext::Job(job, offered)) => {
            assert_eq!(job.stamp_100ns(), 7);
            assert!(
                before <= offered && offered <= after,
                "offered at the offer"
            );
        }
        other => panic!("expected a job, got {other:?}"),
    }
}

/// #223 follow-up: when the frames went out after their offer (p50, p99,
/// max over the window) and how many were late.
#[test]
fn the_send_timing_reaches_the_telemetry() {
    let max = MaxOut::new();
    let status = max.status();
    assert_eq!(
        (
            status.send_at_us_p50,
            status.send_at_us_p99,
            status.send_at_us_max,
            status.send_late
        ),
        (0, 0, 0, 0),
        "nothing sent yet"
    );
    for (at_us, late) in [(10_000, false), (12_000, true), (30_000, true)] {
        max.record_send_timing(SendTiming { at_us, late });
    }
    let status = max.status();
    assert_eq!(
        (
            status.send_at_us_p50,
            status.send_at_us_p99,
            status.send_at_us_max,
            status.send_late
        ),
        (12_000, 30_000, 30_000, 2)
    );
}
