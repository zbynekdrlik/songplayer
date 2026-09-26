//! #212 follow-up: the NDI input never blocks its grid thread on a receiver
//! create or destroy (design record: #212 comment 5849076208). On the box the
//! SDK's close + create blocked the grid thread for ~0.5 s each (comment
//! 5849047061). Here the mock's `recv_create` / `recv_destroy` block for 500 ms
//! too, and the input must still offer exactly one pair on every boundary, the
//! standby pair covering the connect. A child of `ndi_input_tests.rs`, sharing
//! its rig.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::playback::program_bus::{ProgramJob, Take};
use crate::playback::vban_out::VbanClock;

/// How long the mock's receiver create and destroy each block (the box's
/// close took ~0.5 s).
const BLOCK: Duration = Duration::from_millis(500);

/// A `service()` call longer than 8 grid slots would force a resync on the
/// real grid.
const RESYNC_BUDGET: Duration = Duration::from_millis(8 * 1000 / 30);

/// The second source; its frames are all `NEW_FILL`.
const CAM: &str = "CAM (2)";
const NEW_FILL: u8 = 200;

/// The standby black's `(width, first luma byte)`.
const STANDBY: (u32, u8) = (2, 16);

fn cam() -> InputSettings {
    InputSettings {
        enabled: true,
        source: CAM.to_string(),
    }
}

/// A connected input on program whose SDK create / destroy then block 500 ms.
fn slow_rig() -> Rig {
    let rig = rig(source_frames(30, 30), (0..30).map(Some).collect());
    rig.mock.set_blocking(BLOCK, BLOCK);
    rig
}

/// Point the mock at the second source's frames (every capture from now on).
fn serve_new_frames(rig: &Rig) {
    rig.mock.set_video_frames(vec![uyvy(0, 30, 1, NEW_FILL)]);
    rig.mock.set_video_schedule(vec![Some(0)]);
}

/// One boundary as the program got it.
struct Step {
    /// `(width, first luma byte)`: 4 = a received 4×2 frame (its fill), 2 =
    /// the 2×4 standby black.
    picture: (u32, u8),
    /// `connects_pending` right after the boundary.
    pending: u32,
    /// How long the `service()` call took.
    took: Duration,
}

/// Service `b(k)` as the grid thread does: exactly one pair must reach the
/// program (`drain` panics on a fill), stamped on `b(k)`, a standby pair being
/// the exact black + one silent block.
fn step(rig: &mut Rig, k: usize) -> Step {
    let started = Instant::now();
    rig.input.service(b(k), b(k) + 2 * MS, &rig.bus);
    let took = started.elapsed();
    let jobs = drain(&rig.bus);
    assert_eq!(jobs.len(), 1, "boundary {k}: exactly one pair");
    let job = &jobs[0];
    assert_eq!(
        (job.video_tc_100ns, job.audio_tc_100ns),
        (b(k), b(k) + 2 * MS),
        "boundary {k}: stamped on its boundary"
    );
    if job.width == STANDBY.0 {
        assert_standby(job);
    }
    Step {
        picture: (job.width, job.video[0]),
        pending: rig.status().connects_pending,
        took,
    }
}

/// Every boundary of a [`reconnect`] run.
struct Trace {
    pictures: Vec<(u32, u8)>,
    pending: Vec<u32>,
    longest: Duration,
}

impl Trace {
    fn push(&mut self, s: Step) {
        self.pictures.push(s.picture);
        self.pending.push(s.pending);
        self.longest = self.longest.max(s.took);
    }

    /// Boundaries between the change and the new source's first frame.
    fn window(&self, before: usize, after: usize) -> usize {
        self.pictures.len() - before - after
    }
}

/// Service `before` boundaries on the first source, switch the settings to
/// `CAM (2)`, and keep servicing, 5 ms apart, until `after` boundaries carried
/// its frames (bounded: 10 s).
fn reconnect(rig: &mut Rig, before: usize, after: usize) -> Trace {
    let mut trace = Trace {
        pictures: Vec::new(),
        pending: Vec::new(),
        longest: Duration::ZERO,
    };
    for k in 1..=before {
        trace.push(step(rig, k));
    }
    rig.shared.set_settings(cam());
    serve_new_frames(rig);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut k = before;
    while trace.pictures.iter().filter(|p| p.1 == NEW_FILL).count() < after {
        assert!(
            Instant::now() < deadline,
            "the new source never came through"
        );
        k += 1;
        trace.push(step(rig, k));
        thread::sleep(Duration::from_millis(5));
    }
    trace
}

// --- the connect window --------------------------------------------------------

#[test]
fn the_standby_pair_covers_the_connect_window() {
    let mut rig = slow_rig();
    let trace = reconnect(&mut rig, 3, 3);
    let window = trace.window(3, 3);
    assert!(
        window >= 1,
        "the change boundary is served while the connect runs: {:?}",
        trace.pictures
    );
    let mut expected = vec![(4, 0), (4, 1), (4, 2)];
    expected.extend(vec![STANDBY; window]);
    expected.extend(vec![(4, NEW_FILL); 3]);
    assert_eq!(
        trace.pictures, expected,
        "the old source, the standby pair while the connect runs, the new source"
    );
    assert!(
        trace.longest < RESYNC_BUDGET,
        "no boundary waited on the SDK: the longest service() took {:?}",
        trace.longest
    );
    let st = rig.status();
    assert_eq!(st.no_source_boundaries, window as u64);
    assert_eq!(st.boundaries, trace.pictures.len() as u64);
}

#[test]
fn the_new_sources_frames_appear_once_the_connect_completes() {
    let mut rig = slow_rig();
    let trace = reconnect(&mut rig, 3, 3);
    let first_new = trace.pictures.len() - 3;
    assert_eq!(
        trace.pictures[3], STANDBY,
        "the change boundary does not wait for the new receiver"
    );
    assert_eq!(trace.pictures[first_new..], [(4, NEW_FILL); 3]);
    assert_eq!(
        (trace.pending[first_new - 1], trace.pending[first_new]),
        (1, 0),
        "swapped in on the first boundary after the connect finished"
    );
    // The new pair is receiver 3 / FrameSync 4; the old FrameSync 2 captured
    // only before the change.
    assert_eq!(rig.calls_matching("framesync_capture_video(2)"), 3);
    assert_eq!(rig.calls_matching("framesync_capture_video(4)"), 3);
    assert_eq!(rig.calls_matching("recv_create(CAM (2),"), 1);
    let st = rig.status();
    assert!(st.connected);
    assert_eq!(
        (st.frames_received, st.video_repeats),
        (4, 2),
        "three old frames, then the new one (repeated twice)"
    );
    let connect_ms = st.last_connect_ms.expect("the connect is timed");
    assert!(connect_ms >= 500, "the connect took {connect_ms} ms");
}

#[test]
fn connects_pending_reads_1_during_the_connect_and_0_after() {
    let mut rig = slow_rig();
    assert_eq!(rig.status().connects_pending, 0, "the rig's connect landed");
    let trace = reconnect(&mut rig, 3, 3);
    let window = trace.window(3, 3);
    assert!(window >= 1, "a connect was pending: {:?}", trace.pending);
    let mut expected = vec![0; 3];
    expected.extend(vec![1; window]);
    expected.extend(vec![0; 3]);
    assert_eq!(trace.pending, expected);
    wait_for("the old receiver's close is timed", || {
        rig.status().last_close_ms.is_some()
    });
    let json = serde_json::to_value(rig.status()).unwrap();
    assert_eq!(json["connects_pending"], 0);
    let connect_ms = json["last_connect_ms"].as_u64().unwrap();
    let close_ms = json["last_close_ms"].as_u64().unwrap();
    assert!(connect_ms >= 500, "connect {connect_ms} ms");
    assert!(close_ms >= 500, "close {close_ms} ms (off the grid thread)");
}

#[test]
fn a_second_change_during_a_pending_connect_drops_the_first_result() {
    let mut rig = slow_rig();
    let mut longest = Duration::ZERO;
    for k in 1..=3 {
        longest = longest.max(step(&mut rig, k).took);
    }
    // The first change starts B's connect (500 ms) …
    rig.shared.set_settings(InputSettings {
        enabled: true,
        source: "B (1)".to_string(),
    });
    serve_new_frames(&rig);
    let first = step(&mut rig, 4);
    assert_eq!((first.picture, first.pending), (STANDBY, 1));
    // … the second supersedes it while it runs.
    rig.shared.set_settings(cam());
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut news = 0;
    let mut k = 4;
    while news < 3 {
        assert!(Instant::now() < deadline, "CAM (2) never came through");
        k += 1;
        let s = step(&mut rig, k);
        longest = longest.max(s.took);
        if s.picture == (4, NEW_FILL) {
            news += 1;
            assert_eq!(s.pending, 0);
        } else {
            assert_eq!(
                (s.picture, s.pending),
                (STANDBY, 1),
                "boundary {k}: standby while B's and then CAM's connect run"
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        longest < RESYNC_BUDGET,
        "no boundary waited on the SDK: {longest:?}"
    );
    let calls = rig.mock.calls();
    let at = |call: &str| calls.iter().position(|c| c == call).expect(call);
    let creates: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("recv_create"))
        .collect();
    assert_eq!(
        creates,
        [
            "recv_create(CG-OBS (manual),SongPlayer program input)",
            "recv_create(B (1),SongPlayer program input)",
            "recv_create(CAM (2),SongPlayer program input)",
        ]
    );
    assert!(
        at("framesync_create(3)") < at("recv_create(CAM (2),SongPlayer program input)"),
        "one connect in flight: CAM's started only after B's had finished"
    );
    // B's pair (receiver 3 / FrameSync 4) was never swapped in; it is closed
    // off the grid thread. CAM's (FrameSync 6) carries the new frames.
    assert_eq!(rig.calls_matching("framesync_capture_video(4)"), 0);
    assert_eq!(rig.calls_matching("framesync_capture_video(6)"), 3);
    wait_for("B's pair is closed", || {
        rig.mock.calls().iter().any(|c| c == "recv_destroy(3)")
    });
    let calls = rig.mock.calls();
    let at = |call: &str| calls.iter().position(|c| c == call).expect(call);
    assert!(
        at("framesync_destroy(4)") < at("recv_destroy(3)"),
        "the FrameSync first"
    );
    assert!(rig.status().connected);
}

// --- the stop path ------------------------------------------------------------

#[test]
fn the_stop_path_abandons_a_pending_connect_and_its_helper_closes_what_it_made() {
    let mut rig = raw_rig(source_frames(30, 30), vec![Some(0)]);
    rig.mock.set_blocking(BLOCK, BLOCK);
    let s = step(&mut rig, 1);
    assert_eq!((s.picture, s.pending), (STANDBY, 1), "the connect runs");
    let started = Instant::now();
    rig.input.disconnect();
    assert!(
        started.elapsed() < RESYNC_BUDGET,
        "the stop path does not wait for a pending connect"
    );
    assert_eq!(rig.status().connects_pending, 0);
    assert!(rig.input.sync.is_none());
    wait_for("the helper closes the pair nobody took", || {
        rig.mock.calls().last().map(String::as_str) == Some("recv_destroy(1)")
    });
    assert_eq!(
        rig.mock.calls(),
        vec![
            "recv_create(CG-OBS (manual),SongPlayer program input)",
            "framesync_create(1)",
            "framesync_destroy(2)",
            "recv_destroy(1)",
        ]
    );
}

// --- (re)connect + retry (moved from `ndi_input_tests.rs`) ---------------------

#[test]
fn a_source_change_closes_the_receiver_and_opens_the_new_one() {
    let mut rig = rig(source_frames(30, 30), vec![Some(0)]);
    rig.run(1);
    rig.shared.set_settings(cam());
    rig.input.service(b(2), b(2), &rig.bus);
    settle(&mut rig.input, b(2));
    wait_for("the old receiver is closed", || {
        rig.status().last_close_ms.is_some()
    });
    // The close and the connect run on two helper threads: each keeps its own
    // order.
    let only = |kind: &str| -> Vec<String> {
        rig.mock
            .calls()
            .into_iter()
            .filter(|c| c.contains(kind))
            .collect()
    };
    assert_eq!(
        only("_create("),
        vec![
            "recv_create(CG-OBS (manual),SongPlayer program input)",
            "framesync_create(1)",
            "recv_create(CAM (2),SongPlayer program input)",
            "framesync_create(3)",
        ]
    );
    assert_eq!(
        only("_destroy("),
        vec!["framesync_destroy(2)", "recv_destroy(1)"]
    );
    // Disabling closes it again.
    rig.shared.set_settings(InputSettings::default());
    rig.input.service(b(3), b(3), &rig.bus);
    wait_for("the new receiver is closed", || {
        rig.mock.calls().last().map(String::as_str) == Some("recv_destroy(3)")
    });
    assert_eq!(
        only("_destroy("),
        vec![
            "framesync_destroy(2)",
            "recv_destroy(1)",
            "framesync_destroy(4)",
            "recv_destroy(3)",
        ]
    );
}

#[test]
fn a_failed_receiver_is_retried_exactly_5_s_after_the_attempt() {
    let mut rig = raw_rig(source_frames(30, 30), vec![Some(0)]);
    rig.mock.set_fail_create(true, false);
    settle(&mut rig.input, b(1)); // the attempt at b(1) fails
    assert_eq!(rig.input.retry_at, b(1) + INPUT_RECONNECT_100NS);
    let mut standby = Vec::new();
    for k in 1..=150 {
        rig.input.service(b(k), b(k), &rig.bus);
        standby.extend(drain(&rig.bus));
    }
    assert_eq!(rig.calls_matching("recv_create"), 1, "no retry inside 5 s");
    assert_eq!(standby.len(), 150, "standby on every boundary meanwhile");
    standby.iter().for_each(assert_standby);
    rig.mock.set_fail_create(false, false);
    rig.input.service(b(151), b(151), &rig.bus); // b(1) + 5 s exactly
    drain(&rig.bus);
    settle(&mut rig.input, b(151));
    assert_eq!(rig.calls_matching("recv_create"), 2, "retried at b(151)");
    rig.input.service(b(152), b(152), &rig.bus);
    let jobs = drain(&rig.bus);
    assert_eq!(jobs[0].width, 4, "connected: the source's frame");
}

// --- the real grid ------------------------------------------------------------

/// The program wall in real time from just after `b(0)`: the loop services
/// b(1), b(2), … on the real 33.3 ms grid, so a `service()` that blocks misses
/// real boundaries, exactly as on the box.
struct RealClock {
    start: Instant,
}

impl VbanClock for RealClock {
    fn now_100ns(&mut self) -> i64 {
        let elapsed = i64::try_from(self.start.elapsed().as_nanos() / 100).unwrap();
        b(0) + 1_000 + elapsed
    }

    fn sleep_100ns(&mut self, d_100ns: i64) {
        let d = u64::try_from(d_100ns).unwrap_or(0);
        thread::sleep(Duration::from_nanos(d * 100));
    }
}

/// `(value, run length)` of each run of equal values.
fn runs(values: &[u32]) -> Vec<(u32, usize)> {
    let mut out: Vec<(u32, usize)> = Vec::new();
    for &v in values {
        match out.last_mut() {
            Some((last, n)) if *last == v => *n += 1,
            _ => out.push((v, 1)),
        }
    }
    out
}

#[test]
fn a_source_change_on_program_offers_one_pair_per_boundary_without_a_resync() {
    let Rig {
        mock,
        shared,
        bus,
        input,
    } = slow_rig();
    // The `SP-program` sender: every program job, in order.
    let (job_tx, job_rx) = mpsc::channel();
    let sender_bus = bus.clone();
    let sender = thread::spawn(move || {
        loop {
            match sender_bus.take_timeout(Duration::from_millis(20)) {
                Take::Job(job) => job_tx.send(job).unwrap(),
                Take::Idle => {}
                Take::Stopped => break,
            }
        }
    });
    let (done_tx, done_rx) = mpsc::channel();
    let loop_bus = bus.clone();
    thread::spawn(move || {
        let mut input = input;
        let mut clock = RealClock {
            start: Instant::now(),
        };
        run_input_loop(&mut input, &loop_bus, &mut clock);
        let _ = done_tx.send(input);
    });
    let boundaries = || shared.status(&cam()).boundaries;
    wait_for("10 boundaries on the first source", || boundaries() >= 10);
    shared.set_settings(cam());
    wait_for("the new pair is created", || {
        mock.calls().iter().any(|c| c == "framesync_create(3)")
    });
    wait_for("the new pair is swapped in", || shared.is_connected());
    let created = boundaries();
    wait_for("10 more boundaries", || boundaries() >= created + 10);
    shared.stop();
    let _input = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the loop stops on the flag");
    bus.stop();
    sender.join().unwrap();
    let st = shared.status(&cam());
    assert_eq!(
        (st.resyncs, st.relatches),
        (0, 0),
        "the grid thread never blocked on the SDK"
    );
    let jobs: Vec<ProgramJob> = job_rx.try_iter().collect();
    let n = st.boundaries as usize;
    let stamps: Vec<i64> = jobs.iter().map(ProgramJob::stamp_100ns).collect();
    assert_eq!(
        stamps,
        (1..=n).map(b).collect::<Vec<_>>(),
        "one pair on every boundary b(1)..=b({n})"
    );
    let widths: Vec<u32> = jobs
        .iter()
        .map(|job| match job {
            ProgramJob::Source(job) => job.width,
            ProgramJob::Standby { stamp_100ns } => panic!("the program filled {stamp_100ns}"),
        })
        .collect();
    let segments = runs(&widths);
    assert_eq!(
        segments.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![4, 2, 4],
        "the first source, standby while the connect runs, the new source: {segments:?}"
    );
    assert!(segments[0].1 >= 10 && segments[2].1 >= 10, "{segments:?}");
    let health = bus.status().health;
    assert_eq!(
        (health.forwarded, health.filled, health.resyncs),
        (n as u64, 0, 0)
    );
    assert!(st.last_connect_ms.unwrap() >= 500);
    assert!(st.last_close_ms.unwrap() >= 500);
}
