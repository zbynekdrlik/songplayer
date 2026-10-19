//! #279 P1b: `program_ndi_source` on [`MaxOut`]: while NDI comes from the
//! GPU readback the `program-max` thread takes jobs whatever MAX's setting
//! (MAX's own target and sender go while it is off), the queue is dropped
//! only when nothing wants the compositor, and the setting is read and
//! applied with MAX's other settings.
//! Wired via `#[cfg(test)] #[path = "program_max_tests_ndi.rs"] mod tests_ndi;`
//! in `program_max_phase.rs`.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use sp_core::config::{NdiSource, SpoutSend};

use super::{
    DEFAULT_MAX_SIZE, SETTING_PROGRAM_MAX_SIZE, apply_phase_settings, load_max_size,
    load_ndi_source, load_spout_send, program_max_size,
};
use crate::playback::program_max::{MaxJob, MaxNext, MaxOut};

fn black(stamp: i64) -> MaxJob {
    MaxJob::Black { stamp_100ns: stamp }
}

/// The stamp of the ready step's job (`try_next`: never a wait).
fn job_stamp(step: Option<MaxNext>) -> i64 {
    match step {
        Some(MaxNext::Job(job, _)) => job.stamp_100ns(),
        other => panic!("expected a job, got {other:?}"),
    }
}

#[test]
fn ndi_from_the_gpu_keeps_the_thread_taking_jobs_with_max_off() {
    let max = MaxOut::new();
    let _consumer = max.attach();
    assert_eq!(max.ndi_source(), NdiSource::Cpu, "the default");
    assert!(
        !max.accepting(),
        "MAX off, NDI on the canvas: nothing wanted"
    );
    assert!(!max.offer_with(|| black(1)));
    assert_eq!(
        (max.enabled(), max.wants().ndi),
        (false, false),
        "MAX's own output is wanted only by its setting"
    );

    assert!(max.set_ndi_source(NdiSource::Gpu), "cpu → gpu is a change");
    assert!(!max.set_ndi_source(NdiSource::Gpu), "the same value");
    assert!(max.accepting(), "NDI wants the compositor");
    assert!(max.offer_with(|| black(2)));
    assert_eq!(job_stamp(max.try_next(true)), 2);
    assert_eq!(
        (max.enabled(), max.wants().ndi),
        (false, true),
        "MAX's own output is not wanted while it is off"
    );
    assert_eq!(max.status().state, "off", "MAX reads off");
    assert_eq!(max.ndi_status().setting, "gpu");

    // MAX on, then off again: NDI keeps the queued job.
    assert!(max.set_enabled(true));
    assert_eq!((max.enabled(), max.wants().ndi), (true, true));
    assert!(max.offer_with(|| black(3)));
    assert!(max.set_enabled(false));
    assert_eq!(max.queued(), 1, "NDI still wants it");
    assert_eq!(job_stamp(max.try_next(true)), 3);

    // NDI back on the canvas with MAX off: nothing wants it.
    assert!(max.offer_with(|| black(4)));
    assert!(max.set_ndi_source(NdiSource::Cpu));
    assert_eq!(max.queued(), 0, "dropped");
    assert!(!max.accepting());
    assert!(matches!(max.try_next(true), Some(MaxNext::Release)));
    assert!(max.try_next(false).is_none(), "holding nothing: it waits");
    assert_eq!(max.ndi_status().setting, "cpu");
}

/// With MAX on, NDI's switch to the canvas drops nothing.
#[test]
fn ndi_back_on_the_canvas_keeps_max_s_jobs() {
    let max = MaxOut::new();
    let _consumer = max.attach();
    max.set_enabled(true);
    max.set_ndi_source(NdiSource::Gpu);
    assert!(max.offer_with(|| black(1)));
    assert!(max.set_ndi_source(NdiSource::Cpu));
    assert_eq!(max.queued(), 1, "MAX still wants it");
    assert_eq!(job_stamp(max.try_next(true)), 1);
}

/// Stops the MAX it holds when dropped (a failed assertion included), so
/// a waiting thread never outlives its test.
struct StopOnDrop(Arc<MaxOut>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// A holding thread that waits for a job while NDI wants the compositor
/// (MAX off) wakes when NDI goes back to the canvas: `set_ndi_source`
/// notifies it, and it releases what nothing wants.
#[test]
fn a_waiting_thread_wakes_when_nothing_wants_the_compositor_any_more() {
    let max = Arc::new(MaxOut::new());
    let _stop = StopOnDrop(max.clone());
    max.set_ndi_source(NdiSource::Gpu);
    let (tx, rx) = mpsc::channel();
    let thread = {
        let max = max.clone();
        std::thread::spawn(move || {
            let _attached = max.attach();
            let _ = tx.send(max.next(true));
        })
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "NDI wants the compositor and no job came: it waits"
    );
    max.set_ndi_source(NdiSource::Cpu);
    let step = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("woken by the setting");
    assert!(matches!(step, MaxNext::Release), "{step:?}");
    thread.join().expect("the thread");
}

async fn settings_pool() -> sqlx::SqlitePool {
    let pool = crate::db::create_memory_pool().await.unwrap();
    crate::db::run_migrations(&pool).await.unwrap();
    pool
}

#[tokio::test]
async fn the_ndi_source_setting_is_read_and_applied_with_max_s() {
    use crate::db::models::set_setting;
    let pool = settings_pool().await;
    assert_eq!(
        load_ndi_source(&pool).await.unwrap(),
        NdiSource::Cpu,
        "unset"
    );
    let max = MaxOut::new();
    apply_phase_settings(&pool, &max).await;
    assert_eq!(max.ndi_source(), NdiSource::Cpu);
    set_setting(&pool, "program_ndi_source", " gpu ")
        .await
        .unwrap();
    assert_eq!(load_ndi_source(&pool).await.unwrap(), NdiSource::Gpu);
    apply_phase_settings(&pool, &max).await;
    assert_eq!(max.ndi_source(), NdiSource::Gpu, "applied");
    set_setting(&pool, "program_ndi_source", "cpu")
        .await
        .unwrap();
    apply_phase_settings(&pool, &max).await;
    assert_eq!(max.ndi_source(), NdiSource::Cpu, "and back");
}

/// #251 (19.10.2026): `program_max_spout_send` is read with MAX's other
/// settings and applied at once; unset is today's `synced`.
#[tokio::test]
async fn the_spout_hand_off_setting_is_read_and_applied_with_max_s() {
    use crate::db::models::set_setting;
    let pool = settings_pool().await;
    assert_eq!(load_spout_send(&pool).await.unwrap(), SpoutSend::Synced);
    let max = MaxOut::new();
    assert_eq!(max.spout_send(), SpoutSend::Synced, "before any read");
    set_setting(&pool, "program_max_spout_send", "release_then_wait")
        .await
        .unwrap();
    apply_phase_settings(&pool, &max).await;
    assert_eq!(max.spout_send(), SpoutSend::ReleaseThenWait, "applied");
    set_setting(&pool, "program_max_spout_send", "plain")
        .await
        .unwrap();
    apply_phase_settings(&pool, &max).await;
    assert_eq!(max.spout_send(), SpoutSend::Plain, "changed");
    set_setting(&pool, "program_max_spout_send", "")
        .await
        .unwrap();
    apply_phase_settings(&pool, &max).await;
    assert_eq!(max.spout_send(), SpoutSend::Synced, "and back");
}

/// #251 (19.10.2026): MAX's size is the canvas unless the setting names
/// `WxH` with each side even, the width 640..=3840 and the height
/// 360..=2160 (trimmed).
#[test]
fn the_max_size_is_the_canvas_unless_the_setting_names_another() {
    assert_eq!(SETTING_PROGRAM_MAX_SIZE, "program_max_size");
    assert_eq!(DEFAULT_MAX_SIZE, (3840, 2160));
    assert_eq!(program_max_size(None), (3840, 2160), "no setting");
    assert_eq!(program_max_size(Some("1920x1080")), (1920, 1080));
    assert_eq!(
        program_max_size(Some(" 2580x1080\n")),
        (2580, 1080),
        "trimmed"
    );
    assert_eq!(
        program_max_size(Some("640x360")),
        (640, 360),
        "the smallest"
    );
    assert_eq!(
        program_max_size(Some("3840x2160")),
        (3840, 2160),
        "the largest"
    );
    for other in [
        "",
        "1920",
        "1920x",
        "x1080",
        "1920X1080",
        "1920 x 1080",
        "-1920x1080",
        "638x360",
        "640x358",
        "3842x2160",
        "3840x2162",
        "1921x1080",
        "1920x1081",
        "1920x1080x2",
    ] {
        assert_eq!(program_max_size(Some(other)), (3840, 2160), "{other:?}");
    }
}

/// #251: `program_max_size` is read and applied with MAX's other settings.
#[tokio::test]
async fn the_max_size_setting_is_read_and_applied_with_max_s() {
    use crate::db::models::set_setting;
    let pool = settings_pool().await;
    assert_eq!(load_max_size(&pool).await.unwrap(), (3840, 2160));
    let max = MaxOut::new();
    assert_eq!(max.max_size(), (3840, 2160), "before any read");
    set_setting(&pool, "program_max_size", "1920x1080")
        .await
        .unwrap();
    apply_phase_settings(&pool, &max).await;
    assert_eq!(max.max_size(), (1920, 1080), "applied");
    assert_eq!((max.status().width, max.status().height), (1920, 1080));
    set_setting(&pool, "program_max_size", "").await.unwrap();
    apply_phase_settings(&pool, &max).await;
    assert_eq!(max.max_size(), (3840, 2160), "and back");
}
