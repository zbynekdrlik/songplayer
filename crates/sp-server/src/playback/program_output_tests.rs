//! #209 `SP-program` sender: what [`ProgramOutput`] puts on the wire for a
//! forwarded source boundary and for its own standby pair, the per-boundary
//! check timing, and the sender thread end to end on a settable clock.
//! Wired via `#[cfg(test)] #[path = "program_output_tests.rs"] mod tests;`.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramBus, ProgramJob};
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::wallclock::WallClock;
use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};
use std::sync::Arc;
use std::time::Duration;

const T0: i64 = 17_900_000_000_000_000;

fn output(w: u32, h: u32) -> (Arc<MockNdiBackend>, ProgramOutput<MockNdiBackend>) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    (backend, ProgramOutput::new(sender, w, h))
}

#[test]
fn a_standby_pair_is_one_silent_block_then_the_nv12_black_on_its_boundary() {
    let (backend, mut out) = output(4, 2);
    let stamp = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    assert_eq!(
        out.submit(ProgramJob::Standby { stamp_100ns: stamp }, stamp + 123),
        stamp
    );
    assert_eq!(
        backend.calls(),
        vec![
            "send_create_with_clocking(SP-program,false,false)".to_string(),
            "send_audio(42,sr=48000,ch=2,spc=1600)".to_string(),
            "send_video_async(42,NV12,4x2,stride=4,30/1)".to_string(),
        ],
        "a paced sender (no video clocking), audio first, NV12 on the 30/1 grid"
    );
    assert_eq!(backend.video_timecodes(), vec![stamp]);
    assert_eq!(
        backend.audio_timecodes(),
        vec![stamp + 123],
        "audio = emit instant"
    );
    let planar = backend.last_audio_planar();
    assert_eq!(planar.len(), 3200);
    assert!(planar.iter().all(|&s| s == 0.0), "silence");
    assert_eq!(
        backend.last_async_video_slice().map(|(_, len)| len),
        Some(4 * 2 * 3 / 2),
        "the NV12 black of the configured size"
    );
}

#[test]
fn a_forwarded_boundary_keeps_the_sources_frame_audio_and_stamps() {
    let (backend, mut out) = output(4, 2);
    let video = SharedFrame::new(vec![7u8; 8 * 2 * 3 / 2]);
    let stamp = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    let job = SubmitJob {
        width: 8,
        height: 2,
        stride: 8,
        video: video.clone(),
        audio: vec![AudioFrame {
            data: vec![0.5; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp + 77,
    };
    assert_eq!(out.submit(ProgramJob::Source(job), stamp + 999), stamp);
    assert_eq!(backend.video_timecodes(), vec![stamp]);
    assert_eq!(
        backend.audio_timecodes(),
        vec![stamp + 77],
        "the source's own stamp"
    );
    assert_eq!(
        backend.last_async_video_slice(),
        Some((video.as_ptr() as usize, video.len())),
        "the source's own allocation, zero copy"
    );
    assert!(backend.last_audio_planar().iter().all(|&s| s == 0.5));
    assert!(
        backend
            .calls()
            .contains(&"send_video_async(42,NV12,8x2,stride=8,30/1)".to_string())
    );
}

#[test]
fn connections_and_flush_reach_the_program_sender() {
    let (backend, mut out) = output(4, 2);
    backend.set_connection_count(3);
    assert_eq!(out.connections(), 3);
    out.flush();
    assert_eq!(
        backend.calls().last().map(String::as_str),
        Some("send_video_flush(42)")
    );
}

#[test]
fn the_sender_checks_one_ms_after_the_next_boundary() {
    let b0 = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    let b1 = strict_next_boundary_100ns(b0, GENLOCK_GRID_FPS);
    assert_eq!(
        next_check_wait(b0),
        Duration::from_nanos(((b1 - b0 + CHECK_AFTER_BOUNDARY_100NS) * 100) as u64)
    );
    assert_eq!(
        next_check_wait(b0 + 5),
        Duration::from_nanos(((b1 - b0 - 5 + CHECK_AFTER_BOUNDARY_100NS) * 100) as u64)
    );
    assert_eq!(CHECK_AFTER_BOUNDARY_100NS, 10_000, "1 ms");
}

#[test]
fn the_program_wall_ticks_once_per_grid_boundary_not_per_wake() {
    let b = |k: i64| {
        let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
        for _ in 0..k {
            x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
        }
        x
    };
    let mut t = BoundaryTicker::default();
    assert_eq!(t.advance(b(10) + 5), 0, "the first wake only anchors");
    assert_eq!(
        t.advance(b(10) + 20_000),
        0,
        "a second wake in the same slot"
    );
    assert_eq!(t.advance(b(11) + 10_000), 1, "one boundary passed");
    assert_eq!(t.advance(b(11) + 30_000), 0, "woken again, same boundary");
    assert_eq!(
        t.advance(b(14) + 10_000),
        3,
        "a stall: three boundaries owed"
    );
    assert_eq!(t.advance(b(5)), 0, "a backward read owes nothing");
    assert_eq!(
        t.advance(b(15) + 10_000),
        1,
        "and does not move the anchor back"
    );
    assert_eq!(
        t.advance(b(15 + 100)),
        MAX_TICKS_PER_WAKE,
        "a long stall is capped"
    );
    assert_eq!(t.advance(b(116)), 1, "counted from the capped wake");
}

#[test]
fn the_sender_thread_fills_a_sourceless_program_and_stops_flushed() {
    let (backend, out) = output(4, 2);
    let bus = Arc::new(ProgramBus::new());
    let b0 = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    let (wall, clock) = WallClock::settable(b0 + CHECK_AFTER_BOUNDARY_100NS);
    // Every wait is bounded: a mutant that never stops the loop fails this test
    // after 20 s instead of hanging the mutation gate into its 300 s timeout.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let thread = {
        let bus = bus.clone();
        std::thread::spawn(move || {
            let (mut out, mut wall) = (out, wall);
            run_program_loop(&mut out, &bus, &mut wall);
            let _ = done_tx.send(());
            // Hand the output back so its sender outlives the call-log check
            // below (dropping it here would append `send_destroy`).
            out
        })
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while bus.status().health.submitted < 1 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    bus.stop();
    done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the sender thread exits once the bus is stopped");
    let _out = thread.join().unwrap();
    assert_eq!(
        clock.get(),
        b0 + CHECK_AFTER_BOUNDARY_100NS,
        "the clock never moved"
    );
    assert_eq!(
        backend.video_timecodes(),
        vec![b0],
        "exactly the one reached boundary"
    );
    let st = bus.status();
    assert_eq!(st.health.submitted, 1);
    assert_eq!(st.health.filled, 1);
    let calls = backend.calls();
    assert!(calls.contains(&"send_get_no_connections(42,0)".to_string()));
    assert_eq!(
        calls.last().map(String::as_str),
        Some("send_video_flush(42)")
    );
}

#[test]
fn the_sender_thread_ticks_its_wall_once_per_boundary_passed() {
    // The loop wakes several times per boundary (a job, an idle check); its
    // wall must still tick once per grid boundary passed, like the pacer
    // walls, or it slews a UTC step in faster than the stamps (#209 review).
    let (backend, out) = output(4, 2);
    let bus = Arc::new(ProgramBus::new());
    let b0 = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    let b3 = (0..3).fold(b0, |x, _| strict_next_boundary_100ns(x, GENLOCK_GRID_FPS));
    let (wall, clock) = WallClock::settable(b0 + CHECK_AFTER_BOUNDARY_100NS);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let thread = {
        let bus = bus.clone();
        std::thread::spawn(move || {
            let (mut out, mut wall) = (out, wall);
            run_program_loop(&mut out, &bus, &mut wall);
            let _ = done_tx.send(());
            wall
        })
    };
    let wait_for = |n: u64| {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while bus.status().health.submitted < n && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
    };
    wait_for(1); // b0 filled and sent
    clock.set(b3 + CHECK_AFTER_BOUNDARY_100NS); // three boundaries pass at once
    wait_for(4); // b1..b3 filled and sent
    bus.stop();
    done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the sender thread exits once the bus is stopped");
    let wall = thread.join().unwrap();
    assert_eq!(
        backend.video_timecodes().len(),
        4,
        "b0..b3, one pair per boundary"
    );
    assert_eq!(
        wall.frames_since_resample(),
        3,
        "three boundaries passed = three ticks, however often the loop woke"
    );
}
