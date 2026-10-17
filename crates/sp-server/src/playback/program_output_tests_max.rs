//! #223 S2: the `SP-program` sender offers every boundary to
//! `SP-program-MAX` — its NATIVE picture(s), never the canvas — after VBAN's
//! block and before its own NDI submit, and a stalled `program-max` thread
//! never delays VBAN or the NDI submit (it only coalesces).
//! Wired via `#[cfg(test)] #[path = "program_output_tests_max.rs"] mod tests_max;`.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_gpu::FakeRing;
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

use super::ProgramOutput;
use crate::playback::frame_buf::SharedFrame;
use crate::playback::pacer::FrameBlend;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_max::{MaxJob, MaxNext, MaxOut, MaxPicture};
use crate::playback::program_max_worker::tests::{
    Drawn, FakeGpu, Gate, Hold, spawn_loop, wait_until,
};
use crate::playback::program_transition::{MixJob, weight_q8};
use crate::playback::submit_handoff::SubmitJob;
use crate::playback::vban_out::VbanOut;

const T0: i64 = 17_900_000_000_000_000;

/// The k-th grid boundary after `floor(T0)`.
fn at(k: usize) -> i64 {
    let mut x = floor_boundary_100ns(T0, GENLOCK_GRID_FPS);
    for _ in 0..k {
        x = strict_next_boundary_100ns(x, GENLOCK_GRID_FPS);
    }
    x
}

/// A source's boundary pair: a `width`×2 NV12 picture of its own allocation
/// and one silent stereo 1600-frame block.
fn source(width: u32, stamp: i64) -> SubmitJob {
    SubmitJob {
        width,
        height: 2,
        stride: width,
        video: SharedFrame::new(vec![90u8; width as usize * 3]).into(),
        audio: vec![AudioFrame {
            data: vec![0.0; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
        live: true,
        media_pts_100ns: None,
        blend: None,
    }
}

/// A 2×2-canvas program output on a mock NDI sender, feeding `vban` and
/// offering to `max`.
fn output(
    backend: &Arc<MockNdiBackend>,
    vban: &Arc<VbanOut>,
    max: &Arc<MaxOut>,
) -> ProgramOutput<MockNdiBackend> {
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    ProgramOutput::new(sender, 2, 2)
        .with_vban(vban.clone())
        .with_max(max.clone())
}

/// A MAX that takes jobs, and the thread guard that makes it (the test
/// takes the jobs itself with `next`).
fn taking() -> Arc<MaxOut> {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    std::mem::forget(max.attach());
    max
}

/// The job `max` holds next (it must hold one; `try_next` never waits).
fn next_job(max: &MaxOut) -> MaxJob {
    assert_eq!(max.queued(), 1, "one job offered");
    match max.try_next(false) {
        Some(MaxNext::Job(job, _)) => job,
        other => panic!("expected a job, got {other:?}"),
    }
}

fn same_picture(got: &MaxPicture, job: &SubmitJob) -> bool {
    got.video.ptr_eq(&job.video)
        && (got.width, got.height, got.stride) == (job.width, job.height, job.stride)
}

#[test]
fn a_forwarded_boundary_offers_its_native_picture_and_ndi_gets_the_canvas() {
    let backend = Arc::new(MockNdiBackend::new());
    let (vban, max) = (Arc::new(VbanOut::new()), taking());
    let mut out = output(&backend, &vban, &max);
    let job = source(4, at(0));
    out.submit(ProgramJob::Source(job.clone()));
    let MaxJob::Picture {
        stamp_100ns,
        picture,
    } = next_job(&max)
    else {
        panic!("a forwarded boundary is a picture on MAX");
    };
    assert_eq!(stamp_100ns, at(0), "SP-program's stamp");
    assert!(
        same_picture(&picture, &job),
        "the source's own 4x2 frame (an Arc bump), never the canvas"
    );
    let sends: Vec<String> = backend
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("send_video_async("))
        .collect();
    assert_eq!(
        sends,
        ["send_video_async(42,NV12,2x2,stride=2,30/1)"],
        "NDI got the 2x2 canvas, not the 4x2 native picture"
    );
}

#[test]
fn a_ring_frame_goes_to_max_itself_and_the_canvas_counts_a_miss() {
    // #279 P2: a source pair whose picture is a ring frame (P3a's hardware
    // decode). MAX gets the frame itself (an `Arc` bump, sampled in place);
    // `SP-program`'s CPU canvas cannot read it, so NDI sends the canvas
    // black and the miss is counted, never a panic, never a skipped
    // boundary. A CPU frame blended toward a ring frame shows its own side
    // faded toward black (the ring side) and counts that miss too.
    let backend = Arc::new(MockNdiBackend::new());
    let (vban, max) = (Arc::new(VbanOut::new()), taking());
    let mut out = output(&backend, &vban, &max);
    let ring = FakeRing::new(2);
    let frame = ring.frame(4, 2, Duration::ZERO).expect("a free slot");
    let job = SubmitJob {
        video: frame.clone().into(),
        ..source(4, at(0))
    };
    out.submit(ProgramJob::Source(job.clone()));
    let MaxJob::Picture { picture, .. } = next_job(&max) else {
        panic!("a forwarded boundary is a picture on MAX");
    };
    assert!(same_picture(&picture, &job));
    assert!(
        picture.video.gpu().is_some_and(|f| f.ptr_eq(&frame)),
        "the ring frame itself"
    );
    assert_eq!(out.canvas_misses(), 1, "the canvas could not fit it");
    let wire = out.wire_frame().map(|f| f[..].to_vec());
    assert_eq!(
        wire,
        Some(vec![16, 16, 16, 16, 128, 128]),
        "the canvas black"
    );
    let partner = ring.frame(2, 2, Duration::ZERO).expect("a free slot");
    let blended = SubmitJob {
        blend: Some(FrameBlend {
            video: partner.clone().into(),
            weight_q8: 64,
        }),
        ..source(2, at(1))
    };
    out.submit(ProgramJob::Source(blended));
    assert_eq!(out.canvas_misses(), 2, "the blend's ring side");
    // The 2x2 canvas picture (every byte 90) a quarter of the way to the
    // black: (90 * 192 + 16 * 64 + 128) >> 8 = 72, (90 * 192 + 128 * 64 +
    // 128) >> 8 = 100 (the sides swapped would read 35 / 119).
    let wire = out.wire_frame().map(|f| f[..].to_vec());
    assert_eq!(
        wire,
        Some(vec![72, 72, 72, 72, 100, 100]),
        "the CPU side faded toward the ring side's black"
    );
    let MaxJob::Fade { to, .. } = next_job(&max) else {
        panic!("a blended boundary is a fade on MAX");
    };
    let to = to.expect("the blend's next frame");
    assert!(to.video.gpu().is_some_and(|f| f.ptr_eq(&partner)));
    let sends = backend
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("send_video_async("))
        .count();
    assert_eq!(sends, 2, "both boundaries went out");
}

#[test]
fn the_standby_is_black_on_max() {
    let backend = Arc::new(MockNdiBackend::new());
    let (vban, max) = (Arc::new(VbanOut::new()), taking());
    let mut out = output(&backend, &vban, &max);
    out.submit(ProgramJob::Standby { stamp_100ns: at(1) });
    assert!(matches!(
        next_job(&max),
        MaxJob::Black { stamp_100ns } if stamp_100ns == at(1)
    ));
}

#[test]
fn a_fade_boundary_offers_both_native_pictures_and_its_weight() {
    let backend = Arc::new(MockNdiBackend::new());
    let (vban, max) = (Arc::new(VbanOut::new()), taking());
    let mut out = output(&backend, &vban, &max);
    let (from, to) = (source(4, at(2)), source(8, at(2)));
    out.submit(ProgramJob::Mix(MixJob {
        stamp_100ns: at(2),
        from: Some(from.clone()),
        to: Some(to.clone()),
        slot: 4,
        n_slots: 9,
    }));
    let MaxJob::Fade {
        stamp_100ns,
        from: Some(got_from),
        to: Some(got_to),
        weight_q8: weight,
    } = next_job(&max)
    else {
        panic!("a two-sided fade on MAX");
    };
    assert_eq!(stamp_100ns, at(2));
    assert!(
        same_picture(&got_from, &from),
        "the outgoing native picture"
    );
    assert!(same_picture(&got_to, &to), "the incoming native picture");
    assert_eq!(weight, weight_q8(4, 9), "SP-program's weight");
    assert_eq!(weight, 128, "slot 4 of 9 is the middle");

    out.submit(ProgramJob::Mix(MixJob {
        stamp_100ns: at(3),
        from: None,
        to: Some(source(8, at(3))),
        slot: 0,
        n_slots: 9,
    }));
    assert!(matches!(
        next_job(&max),
        MaxJob::Fade {
            from: None,
            to: Some(_),
            weight_q8: 14,
            ..
        }
    ));
}

#[test]
fn nothing_is_offered_while_max_is_off_and_the_program_still_goes_out() {
    let backend = Arc::new(MockNdiBackend::new());
    let (vban, max) = (Arc::new(VbanOut::new()), taking());
    max.set_enabled(false);
    let mut out = output(&backend, &vban, &max);
    out.submit(ProgramJob::Source(source(4, at(0))));
    out.submit(ProgramJob::Standby { stamp_100ns: at(1) });
    assert_eq!(max.queued(), 0, "MAX is off: nothing offered");
    assert_eq!(max.status().coalesced, 0);
    assert_eq!(backend.video_timecodes(), [at(0), at(1)], "NDI unchanged");
    assert_eq!(vban.queued(), 2, "VBAN unchanged");
}

/// What VBAN, NDI and MAX had when the program thread was inside its MAX
/// offer.
#[derive(Debug, PartialEq, Eq)]
struct AtOffer {
    vban_blocks: usize,
    ndi_video: usize,
    ndi_audio: usize,
}

/// Serve `job` and record what was done by the time of its MAX offer.
fn at_the_offer(job: ProgramJob) -> Vec<AtOffer> {
    let backend = Arc::new(MockNdiBackend::new());
    let (vban, max) = (Arc::new(VbanOut::new()), taking());
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let (backend, vban, seen) = (backend.clone(), vban.clone(), seen.clone());
        max.set_on_offer(move || {
            seen.lock().unwrap().push(AtOffer {
                vban_blocks: vban.queued(),
                ndi_video: backend.video_timecodes().len(),
                ndi_audio: backend.audio_timecodes().len(),
            });
        });
    }
    let mut out = output(&backend, &vban, &max);
    out.submit(job);
    assert_eq!(max.queued(), 1, "the boundary reached MAX");
    assert_eq!(backend.video_timecodes().len(), 1, "and NDI after it");
    std::mem::take(&mut *seen.lock().unwrap())
}

#[test]
fn a_boundary_reaches_vban_before_its_max_offer_and_max_before_its_ndi_submit() {
    let mix = MixJob {
        stamp_100ns: at(2),
        from: Some(source(4, at(2))),
        to: Some(source(8, at(2))),
        slot: 1,
        n_slots: 9,
    };
    let jobs = [
        ("a forwarded pair", ProgramJob::Source(source(4, at(0)))),
        ("the standby", ProgramJob::Standby { stamp_100ns: at(1) }),
        ("a fade boundary", ProgramJob::Mix(mix)),
    ];
    for (what, job) in jobs {
        assert_eq!(
            at_the_offer(job),
            [AtOffer {
                vban_blocks: 1,
                ndi_video: 0,
                ndi_audio: 0,
            }],
            "{what}: VBAN already had its block, NDI nothing yet"
        );
    }
}

/// Opens the gate when dropped, a failed assertion included.
struct Opener(Arc<Gate>);

impl Drop for Opener {
    fn drop(&mut self) {
        self.0.open();
    }
}

#[test]
fn a_stalled_max_thread_never_delays_vban_or_the_ndi_submit() {
    let max = Arc::new(MaxOut::new());
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let gate = Arc::new(Gate::default());
    let opener = Opener(gate.clone());
    let (entered_tx, entered_rx) = mpsc::channel();
    gpu.hold_next_compose(Hold {
        entered: entered_tx,
        gate: gate.clone(),
    });
    let consumer = spawn_loop(&max, &gpu);
    wait_until("the program-max thread takes jobs", || max.accepting());

    let backend = Arc::new(MockNdiBackend::new());
    let vban = Arc::new(VbanOut::new());
    let mut out = output(&backend, &vban, &max);
    // Boundary k shows a (2k + 2)×2 picture, so MAX's draws name it.
    out.submit(ProgramJob::Source(source(2, at(0))));
    entered_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the program-max thread is composing boundary 0, held");

    let (done_tx, done_rx) = mpsc::channel();
    let program = std::thread::spawn(move || {
        for k in 1..=6 {
            out.submit(ProgramJob::Source(source(2 * k as u32 + 2, at(k))));
        }
        let _ = done_tx.send(());
        out
    });
    done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("six more boundaries went out while MAX was held");
    let _out = program.join().expect("the program thread");
    assert_eq!(
        backend.video_timecodes(),
        (0..=6).map(at).collect::<Vec<_>>(),
        "every NDI submit went out"
    );
    assert_eq!(vban.queued(), 7, "VBAN got every block");
    assert_eq!(max.queued(), 2, "two boundaries wait for MAX");
    assert_eq!(max.status().coalesced, 4, "boundaries 1 to 4 were dropped");

    drop(opener);
    wait_until("MAX catches up", || max.status().submitted == 3);
    let widths: Vec<u32> = gpu
        .log()
        .drawn
        .iter()
        .map(|drawn| match drawn {
            Drawn::Picture(p) => p.width,
            other => panic!("a forwarded boundary draws a picture: {other:?}"),
        })
        .collect();
    assert_eq!(widths, [2, 12, 14], "boundary 0, then the newest two");
    max.stop();
    wait_until("the program-max thread exits", || consumer.is_finished());
    consumer.join().expect("the program-max thread");
}

/// #251: a pair the pacer blended toward the next frame — a 2×2 canvas
/// picture, so the canvas reads both frames as they are.
fn blended(stamp: i64, next: &SharedFrame, weight_q8: u32) -> SubmitJob {
    let mut job = source(2, stamp);
    job.blend = Some(FrameBlend {
        video: next.clone().into(),
        weight_q8,
    });
    job
}

#[test]
fn a_blended_pair_offers_its_two_frames_as_a_fade_on_max() {
    let backend = Arc::new(MockNdiBackend::new());
    let (vban, max) = (Arc::new(VbanOut::new()), taking());
    let mut out = output(&backend, &vban, &max);
    let next = SharedFrame::new(vec![200u8; 6]);
    let job = blended(at(0), &next, 85);
    out.submit(ProgramJob::Source(job.clone()));
    let MaxJob::Fade {
        stamp_100ns,
        from: Some(from),
        to: Some(to),
        weight_q8,
    } = next_job(&max)
    else {
        panic!("a blended pair is a fade of its two frames on MAX");
    };
    assert_eq!(stamp_100ns, at(0));
    assert!(same_picture(&from, &job), "the shown frame, an Arc bump");
    assert!(
        to.video.expect_cpu().ptr_eq(&next),
        "the next frame, an Arc bump"
    );
    assert_eq!(
        (to.width, to.height, to.stride),
        (2, 2, 2),
        "the next frame in the shown frame's layout"
    );
    assert_eq!(weight_q8, 85, "the pacer's weight");
}

#[test]
fn a_blended_pair_goes_out_on_ndi_as_one_mixed_canvas_picture() {
    let backend = Arc::new(MockNdiBackend::new());
    let (vban, max) = (Arc::new(VbanOut::new()), taking());
    let mut out = output(&backend, &vban, &max);
    let next = SharedFrame::new(vec![200u8; 6]);
    out.submit(ProgramJob::Source(blended(at(0), &next, 128)));
    let held = out.submitter.held_frame().expect("a picture was sent");
    assert_eq!(
        &held[..],
        [145u8; 6],
        "(90·128 + 200·128 + 128) >> 8 on every byte"
    );

    let plain = source(2, at(1));
    out.submit(ProgramJob::Source(plain.clone()));
    let held = out.submitter.held_frame().expect("a picture was sent");
    assert!(
        held.ptr_eq(plain.video.expect_cpu()),
        "an unblended canvas picture still goes out as it is"
    );
}
