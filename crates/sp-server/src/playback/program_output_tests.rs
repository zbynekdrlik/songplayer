//! #209 `SP-program` sender: what [`ProgramOutput`] puts on the wire for a
//! forwarded source boundary and for its own standby pair, the per-boundary
//! check timing, and the sender thread end to end on a settable clock. #215:
//! a mixed window boundary (the crossfaded block, the blended or midpoint-cut
//! picture, a missing side as black + silence).
//! Wired via `#[cfg(test)] #[path = "program_output_tests.rs"] mod tests;`.

use super::*;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramBus, ProgramJob};
use crate::playback::program_transition::{Layout, MixJob, crossfade_gains};
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

// ---- #215: one boundary inside a transition window (`ProgramJob::Mix`) ------

/// A known 4×2 NV12 picture: 8 luma bytes, then 4 interleaved chroma bytes.
const FROM_4X2: [u8; 12] = [16, 32, 64, 100, 128, 200, 235, 0, 128, 128, 90, 240];
const TO_4X2: [u8; 12] = [235, 16, 64, 101, 0, 255, 16, 255, 16, 240, 128, 128];
const LAYOUT_4X2: Layout = Layout {
    width: 4,
    height: 2,
    stride: 4,
    len: 12,
};

/// The k-th grid boundary after `floor(T0)`.
fn at(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

/// A source's boundary pair: a `width`×2 NV12 picture of `pixels` and one
/// stereo 1600-frame block carrying `level`, its audio stamped `audio_tc`.
fn pair(width: u32, pixels: &[u8], stamp: i64, audio_tc: i64, level: f32) -> SubmitJob {
    SubmitJob {
        width,
        height: 2,
        stride: width,
        video: SharedFrame::new(pixels.to_vec()),
        audio: vec![AudioFrame {
            data: vec![level; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: audio_tc,
    }
}

fn mix_at(
    stamp: i64,
    from: Option<SubmitJob>,
    to: Option<SubmitJob>,
    slot: u32,
    n_slots: u32,
) -> MixJob {
    MixJob {
        stamp_100ns: stamp,
        from,
        to,
        slot,
        n_slots,
    }
}

/// Every sample of the last block sent (both channels) is `want(frame)`.
fn assert_block(backend: &MockNdiBackend, want: impl Fn(u64) -> f32) {
    let planar = backend.last_audio_planar();
    assert_eq!(planar.len(), 3200, "one stereo 1600-frame block");
    let (left, right) = planar.split_at(1600);
    for (i, (&l, &r)) in left.iter().zip(right).enumerate() {
        let w = want(i as u64);
        assert_eq!((l, r), (w, w), "frame {i}");
    }
}

/// The video send calls, in order.
fn pictures(backend: &MockNdiBackend) -> Vec<String> {
    backend
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("send_video_async("))
        .collect()
}

#[test]
fn a_mix_submits_the_crossfaded_block_then_the_blended_picture_on_its_boundary() {
    let (backend, mut out) = output(4, 2);
    let stamp = at(0);
    let from = pair(4, &FROM_4X2, stamp, stamp + 11, 0.25);
    let to = pair(4, &TO_4X2, stamp, stamp + 22, 0.5);
    let sources = [from.video.clone(), to.video.clone()];
    let mix = mix_at(stamp, Some(from), Some(to), 4, 9);
    assert_eq!(out.submit(ProgramJob::Mix(mix), stamp + 999), stamp);
    assert_eq!(
        backend.calls(),
        vec![
            "send_create_with_clocking(SP-program,false,false)".to_string(),
            "send_audio(42,sr=48000,ch=2,spc=1600)".to_string(),
            "send_video_async(42,NV12,4x2,stride=4,30/1)".to_string(),
        ],
        "audio first, then the picture, like any boundary"
    );
    assert_eq!(backend.video_timecodes(), vec![stamp]);
    assert_eq!(
        backend.audio_timecodes(),
        vec![stamp + 22],
        "the incoming source's own audio stamp"
    );
    // Slot 4 of a 9-slot window: samples 6400.. of 14 400 on the one curve.
    assert_block(&backend, |i| {
        let (g_from, g_to) = crossfade_gains(4 * 1600 + i, 9 * 1600);
        g_from * 0.25 + g_to * 0.5
    });
    let (ptr, len) = backend.last_async_video_slice().expect("a picture");
    assert_eq!(len, 12);
    assert!(
        sources.iter().all(|s| s.as_ptr() as usize != ptr),
        "the blend is its own buffer, never a source's"
    );
    assert!(!out.size_cut);
}

#[test]
fn the_mix_picture_blends_equal_layouts_at_the_boundarys_weight() {
    let (_backend, mut out) = output(4, 2);
    let stamp = at(0);
    let from = pair(4, &FROM_4X2, stamp, stamp, 0.0);
    let to = pair(4, &TO_4X2, stamp, stamp, 0.0);
    let half = mix_at(stamp, Some(from.clone()), Some(to.clone()), 4, 9);
    let (layout, picture) = out.mix_picture(&half).expect("a picture");
    assert_eq!(layout, LAYOUT_4X2);
    assert_eq!(
        picture.to_vec(),
        vec![126, 24, 64, 101, 64, 228, 126, 128, 72, 184, 109, 184],
        "slot 4 of 9 = weight ½: (f + t + 1) / 2 on luma and chroma alike"
    );
    let first = mix_at(stamp, Some(from), Some(to), 0, 9);
    let (_, picture) = out.mix_picture(&first).expect("a picture");
    assert_eq!(
        picture.to_vec(),
        vec![28, 31, 64, 100, 121, 203, 223, 14, 122, 134, 92, 234],
        "slot 0 of 9 = weight 14/256"
    );
    assert!(!out.size_cut);
}

#[test]
fn a_missing_side_mixes_against_the_black_of_the_present_sides_size() {
    let (backend, mut out) = output(2, 2);
    // The incoming side is missing (its source stalled past the grace).
    let from_only = mix_at(
        at(0),
        Some(pair(4, &FROM_4X2, at(0), at(0) + 11, 0.25)),
        None,
        4,
        9,
    );
    let (layout, picture) = out.mix_picture(&from_only).expect("a picture");
    assert_eq!(
        layout, LAYOUT_4X2,
        "the present side's size, not the program's 2×2 standby"
    );
    assert_eq!(
        picture.to_vec(),
        vec![16, 24, 40, 58, 72, 108, 126, 8, 128, 128, 109, 184],
        "½ of the outgoing picture over studio black (Y 16, UV 128)"
    );
    // The outgoing side is missing (a fade up from nothing).
    let to_only = mix_at(
        at(1),
        None,
        Some(pair(4, &TO_4X2, at(1), at(1) + 22, 0.5)),
        0,
        9,
    );
    let (layout, picture) = out.mix_picture(&to_only).expect("a picture");
    assert_eq!(layout, LAYOUT_4X2);
    assert_eq!(
        picture.to_vec(),
        vec![28, 16, 19, 21, 15, 29, 16, 29, 122, 134, 128, 128],
        "14/256 of the incoming picture over studio black"
    );
    assert!(!out.size_cut, "a black of the same size blends");
    assert!(
        out.mix_picture(&mix_at(at(2), None, None, 0, 9)).is_none(),
        "neither side: no mixed picture"
    );

    // Submitted: the missing side is silence, and the audio keeps the stamp of
    // the side that is there.
    assert_eq!(out.submit(ProgramJob::Mix(from_only), at(0) + 999), at(0));
    assert_block(&backend, |i| {
        let (g_from, g_to) = crossfade_gains(4 * 1600 + i, 9 * 1600);
        g_from * 0.25 + g_to * 0.0
    });
    assert_eq!(out.submit(ProgramJob::Mix(to_only), at(1) + 999), at(1));
    assert_block(&backend, |i| {
        let (g_from, g_to) = crossfade_gains(i, 9 * 1600);
        g_from * 0.0 + g_to * 0.5
    });
    // A mix with neither side (the bus never queues one) goes out as the
    // program's own standby pair.
    let neither = mix_at(at(2), None, None, 5, 9);
    assert_eq!(out.submit(ProgramJob::Mix(neither), at(2) + 5), at(2));
    assert_block(&backend, |_| 0.0);
    assert_eq!(backend.video_timecodes(), vec![at(0), at(1), at(2)]);
    assert_eq!(
        backend.audio_timecodes(),
        vec![at(0) + 11, at(1) + 22, at(2) + 5],
        "the outgoing side's stamp, the incoming side's, the emit instant"
    );
    assert_eq!(
        pictures(&backend),
        vec![
            "send_video_async(42,NV12,4x2,stride=4,30/1)".to_string(),
            "send_video_async(42,NV12,4x2,stride=4,30/1)".to_string(),
            "send_video_async(42,NV12,2x2,stride=2,30/1)".to_string(),
        ]
    );
}

/// A 4×2 NV12 picture with a padded stride of 6 (as a decoder may lock it):
/// two 6-byte luma rows, then one 6-byte chroma row; the padding bytes are 7.
const PADDED_4X2: [u8; 18] = [
    16, 32, 64, 100, 7, 7, 128, 200, 235, 0, 7, 7, 128, 128, 90, 240, 7, 7,
];
const LAYOUT_PADDED: Layout = Layout {
    width: 4,
    height: 2,
    stride: 6,
    len: 18,
};

fn padded(stamp: i64, level: f32) -> SubmitJob {
    SubmitJob {
        stride: 6,
        ..pair(4, &PADDED_4X2, stamp, stamp, level)
    }
}

#[test]
fn a_missing_side_is_black_in_the_present_sides_exact_layout_padding_included() {
    let (_backend, mut out) = output(2, 2);
    // The incoming side stalled: the outgoing padded picture fades to black.
    let from_only = mix_at(at(0), Some(padded(at(0), 0.25)), None, 4, 9);
    let (layout, picture) = out.mix_picture(&from_only).expect("a picture");
    assert_eq!(
        layout, LAYOUT_PADDED,
        "the present side's layout, stride included"
    );
    assert_eq!(
        picture.to_vec(),
        vec![
            16, 24, 40, 58, 12, 12, 72, 108, 126, 8, 12, 12, 128, 128, 109, 184, 68, 68
        ],
        "½ over a black of the same layout: Y 16 over stride × height, then UV 128"
    );
    // A fade up from nothing into a padded source blends too, never a cut.
    let to_only = mix_at(at(1), None, Some(padded(at(1), 0.5)), 0, 9);
    let (layout, picture) = out.mix_picture(&to_only).expect("a picture");
    assert_eq!(layout, LAYOUT_PADDED);
    assert_eq!(
        picture.to_vec(),
        vec![
            16, 17, 19, 21, 16, 16, 22, 26, 28, 15, 16, 16, 128, 128, 126, 134, 121, 121
        ],
        "14/256 of the incoming picture over black"
    );
    assert!(!out.size_cut, "a black of the same layout always blends");
}

#[test]
fn different_sizes_cut_the_picture_at_the_windows_midpoint_once_per_run() {
    let (backend, mut out) = output(2, 2);
    let from = pair(4, &FROM_4X2, at(0), at(0), 0.25);
    let wide = pair(8, &[7u8; 24], at(0), at(0), 0.5);
    let before = mix_at(at(0), Some(from.clone()), Some(wide.clone()), 3, 9);
    let (layout, picture) = out.mix_picture(&before).expect("a picture");
    assert_eq!(
        layout, LAYOUT_4X2,
        "slot 3 of 9 (weight 100): the outgoing picture"
    );
    assert!(picture.ptr_eq(&from.video), "that picture itself, no copy");
    assert!(out.size_cut, "a size cut is running");
    let after = mix_at(at(0), Some(from.clone()), Some(wide.clone()), 4, 9);
    let (layout, picture) = out.mix_picture(&after).expect("a picture");
    assert_eq!(
        layout,
        Layout {
            width: 8,
            height: 2,
            stride: 8,
            len: 24,
        },
        "slot 4 of 9 (weight ½): the incoming picture"
    );
    assert!(picture.ptr_eq(&wide.video));
    assert!(out.size_cut);

    // Any boundary that is not mixed ends the run, so the next window's size
    // cut is logged again.
    out.submit(ProgramJob::Standby { stamp_100ns: at(1) }, at(1));
    assert!(!out.size_cut);
    let next = mix_at(at(2), Some(from.clone()), Some(wide.clone()), 3, 9);
    out.submit(ProgramJob::Mix(next), at(2));
    assert!(out.size_cut);
    let last = mix_at(at(3), Some(from.clone()), Some(wide), 4, 9);
    out.submit(ProgramJob::Mix(last), at(3));
    // The audio still crossfades on the window's curve.
    assert_block(&backend, |i| {
        let (g_from, g_to) = crossfade_gains(4 * 1600 + i, 9 * 1600);
        g_from * 0.25 + g_to * 0.5
    });
    assert_eq!(
        pictures(&backend),
        vec![
            "send_video_async(42,NV12,2x2,stride=2,30/1)".to_string(),
            "send_video_async(42,NV12,4x2,stride=4,30/1)".to_string(),
            "send_video_async(42,NV12,8x2,stride=8,30/1)".to_string(),
        ]
    );
    // Two pictures of the same size blend: that ends the run too.
    let same = mix_at(at(4), Some(from.clone()), Some(from), 4, 9);
    assert!(out.mix_picture(&same).is_some());
    assert!(!out.size_cut);
}
