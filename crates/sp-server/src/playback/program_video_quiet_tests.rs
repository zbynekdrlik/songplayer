//! #251 step 4: the quiet window of the `SP-program` video side — the pure
//! decision, the shared state and the wait, and (`serve`) that the wait
//! comes after VBAN's hand-off and before the NDI submit.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sp_core::config::NdiSource;
use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns};
use sp_gpu::{VBLANK_STALE, VblankGrid};
use sp_ndi::NdiSender;
use sp_ndi::test_util::MockNdiBackend;

use super::{QuietWindow, VideoQuiet, VideoQuietStatus, video_start, wait_out};
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_max::MaxOut;
use crate::playback::program_max_send::SendClock;
use crate::playback::program_output::ProgramOutput;
use crate::playback::vban_out::VbanOut;

/// 60.000 Hz.
const PERIOD: Duration = Duration::from_nanos(16_666_667);

fn us(n: u64) -> Duration {
    Duration::from_micros(n)
}

fn window() -> QuietWindow {
    QuietWindow {
        from: us(3_000),
        to: us(10_500),
    }
}

/// A grid whose vblanks lie on `vblank` (read five refreshes earlier).
fn grid_through(vblank: Instant) -> VblankGrid {
    VblankGrid {
        at: vblank - PERIOD * 5,
        period: PERIOD,
    }
}

#[test]
fn no_grid_means_no_wait() {
    let now = Instant::now();
    assert_eq!(video_start(now, None, window()), None);
}

#[test]
fn a_video_side_starting_inside_the_window_waits_until_its_end() {
    let v = Instant::now();
    let g = Some(grid_through(v));
    let w = window();
    assert_eq!(video_start(v, g, w), None, "on the vblank");
    assert_eq!(video_start(v + us(2_999), g, w), None, "just before");
    assert_eq!(
        video_start(v + us(3_000), g, w),
        Some(v + us(10_500)),
        "its start"
    );
    assert_eq!(video_start(v + us(7_250), g, w), Some(v + us(10_500)));
    assert_eq!(video_start(v + us(10_499), g, w), Some(v + us(10_500)));
    assert_eq!(video_start(v + us(10_500), g, w), None, "its end");
    assert_eq!(video_start(v + us(16_000), g, w), None, "after it");
    // The next refresh's window.
    assert_eq!(
        video_start(v + PERIOD + us(4_000), g, w),
        Some(v + PERIOD + us(10_500))
    );
}

#[test]
fn a_window_that_is_off_or_longer_than_a_refresh_never_waits() {
    let v = Instant::now();
    let g = Some(grid_through(v));
    let at = v + us(5_000);
    let empty = QuietWindow {
        from: us(5_000),
        to: us(5_000),
    };
    assert_eq!(video_start(at, g, empty), None, "start = end: off");
    let backwards = QuietWindow {
        from: us(9_000),
        to: us(4_000),
    };
    assert_eq!(video_start(at, g, backwards), None, "start after end: off");
    let past = QuietWindow {
        from: us(3_000),
        to: PERIOD + us(1),
    };
    assert_eq!(video_start(at, g, past), None, "past one refresh: off");
    // Up to the end of the refresh is a window.
    let whole = QuietWindow {
        from: us(3_000),
        to: PERIOD,
    };
    assert_eq!(video_start(v + us(16_000), g, whole), Some(v + PERIOD));
}

#[test]
fn the_window_is_the_settings_and_a_change_is_told() {
    let quiet = VideoQuiet::default();
    assert_eq!(quiet.window(), window(), "3 to 10.5 ms by default");
    assert!(!quiet.set_window_us(3_000, 10_500), "the same window");
    assert!(quiet.set_window_us(2_000, 10_500), "a new start");
    assert!(quiet.set_window_us(2_000, 9_000), "a new end");
    assert_eq!(
        quiet.window(),
        QuietWindow {
            from: us(2_000),
            to: us(9_000)
        }
    );
}

#[test]
fn only_a_fresh_published_grid_is_waited_on() {
    let quiet = VideoQuiet::default();
    let v = Instant::now();
    let inside = v + us(5_000);
    assert_eq!(quiet.start_at(inside), None, "nothing published");
    quiet.publish_grid(Some(grid_through(v)), inside);
    assert_eq!(quiet.start_at(inside), Some(v + us(10_500)), "read now");
    quiet.publish_grid(Some(grid_through(v)), inside - VBLANK_STALE);
    assert_eq!(
        quiet.start_at(inside),
        Some(v + us(10_500)),
        "read just within the stale bound"
    );
    quiet.publish_grid(Some(grid_through(v)), inside - VBLANK_STALE - us(1));
    assert_eq!(quiet.start_at(inside), None, "a stale grid");
    quiet.publish_grid(None, inside);
    assert_eq!(quiet.start_at(inside), None, "MAX paced on no grid");
}

#[test]
fn every_wait_is_counted_with_the_longest() {
    let quiet = VideoQuiet::default();
    quiet.record_wait(us(2_000));
    quiet.record_wait(us(5_000));
    quiet.record_wait(us(1_000));
    let v = Instant::now();
    quiet.publish_grid(Some(grid_through(v)), v);
    assert_eq!(
        quiet.status(v),
        VideoQuietStatus {
            from_us: 3_000,
            to_us: 10_500,
            grid: true,
            waits: 3,
            wait_us_max: 5_000,
        }
    );
    assert!(!quiet.status(v + VBLANK_STALE + us(1)).grid, "stale");
    let json = serde_json::to_value(quiet.status(v)).unwrap();
    for key in ["from_us", "to_us", "grid", "waits", "wait_us_max"] {
        assert!(json.get(key).is_some(), "{key} in {json}");
    }
}

/// A clock that never moves and records each wait: when, and what NDI and
/// VBAN held by then.
struct Recording {
    now: Instant,
    backend: Arc<MockNdiBackend>,
    vban: Arc<VbanOut>,
    /// (the instant waited for, NDI audio sends so far, VBAN blocks queued)
    log: Arc<Mutex<Vec<(Instant, usize, usize)>>>,
}

impl SendClock for Recording {
    fn now(&mut self) -> Instant {
        self.now
    }

    fn wait_until(&mut self, due: Instant) {
        let sends = self.backend.audio_timecodes().len();
        let queued = self.vban.queued();
        self.log.lock().unwrap().push((due, sends, queued));
    }
}

#[test]
fn wait_out_waits_inside_the_window_and_counts_it() {
    let backend = Arc::new(MockNdiBackend::new());
    let vban = Arc::new(VbanOut::new());
    let log = Arc::new(Mutex::new(Vec::new()));
    let v = Instant::now();
    let mut clock = Recording {
        now: v + us(4_000),
        backend,
        vban,
        log: log.clone(),
    };
    let quiet = VideoQuiet::default();
    quiet.publish_grid(Some(grid_through(v)), v);
    assert_eq!(wait_out(&quiet, &mut clock), Some(us(6_500)));
    assert_eq!(log.lock().unwrap()[0].0, v + us(10_500));
    clock.now = v + us(12_000);
    assert_eq!(wait_out(&quiet, &mut clock), None, "outside: no wait");
    assert_eq!(log.lock().unwrap().len(), 1);
    let status = quiet.status(v);
    assert_eq!((status.waits, status.wait_us_max), (1, 6_500));
}

/// The boundary's VBAN block is handed over BEFORE the wait, and the NDI
/// submit (the video side) only AFTER it.
#[test]
fn serve_waits_after_the_vban_hand_off_and_before_the_ndi_submit() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let vban = Arc::new(VbanOut::new());
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    let v = Instant::now();
    max.video.publish_grid(Some(grid_through(v)), v);
    let log = Arc::new(Mutex::new(Vec::new()));
    let clock = Recording {
        now: v + us(5_000),
        backend: backend.clone(),
        vban: vban.clone(),
        log: log.clone(),
    };
    let mut out = ProgramOutput::new(sender, 2, 2)
        .with_vban(vban.clone())
        .with_max(max.clone())
        .with_video_clock(Box::new(clock));
    out.submit(ProgramJob::Standby {
        stamp_100ns: floor_boundary_100ns(17_900_000_000_000_000, GENLOCK_GRID_FPS),
    });
    let waits = log.lock().unwrap().clone();
    assert_eq!(waits.len(), 1, "one wait: {waits:?}");
    let (due, ndi_sends, vban_queued) = waits[0];
    assert_eq!(due, v + us(10_500), "until the window's end");
    assert_eq!(vban_queued, 1, "VBAN had its block before the wait");
    assert_eq!(ndi_sends, 0, "the NDI submit came after the wait");
    assert_eq!(backend.audio_timecodes().len(), 1, "then the NDI submit");
    assert_eq!(max.video.status(v).waits, 1);
}

/// #279 P1b: with MAX off and NDI from the GPU the compositor still paces
/// (a fresh grid), but no Spout output goes out for Arena to read: the
/// video side never waits. With MAX on it waits as before.
#[test]
fn with_max_off_the_video_side_never_waits_for_arena() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let vban = Arc::new(VbanOut::new());
    let max = Arc::new(MaxOut::new());
    max.set_ndi_source(NdiSource::Gpu);
    let v = Instant::now();
    max.video.publish_grid(Some(grid_through(v)), v);
    let log = Arc::new(Mutex::new(Vec::new()));
    let clock = Recording {
        now: v + us(5_000),
        backend: backend.clone(),
        vban: vban.clone(),
        log: log.clone(),
    };
    let mut out = ProgramOutput::new(sender, 2, 2)
        .with_vban(vban.clone())
        .with_max(max.clone())
        .with_video_clock(Box::new(clock));
    let b0 = floor_boundary_100ns(17_900_000_000_000_000, GENLOCK_GRID_FPS);
    out.submit(ProgramJob::Standby { stamp_100ns: b0 });
    assert!(log.lock().unwrap().is_empty(), "MAX off: no wait");
    assert_eq!(max.video.status(v).waits, 0);
    max.set_enabled(true);
    out.submit(ProgramJob::Standby {
        stamp_100ns: b0 + 333_333,
    });
    assert_eq!(log.lock().unwrap().len(), 1, "MAX on: the wait");
}
