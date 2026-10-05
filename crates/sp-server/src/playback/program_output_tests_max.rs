//! #223 S2: the `SP-program` sender offers every boundary to
//! `SP-program-MAX` — its NATIVE picture(s), never the canvas — after VBAN's
//! block and before its own NDI submit, and a stalled `program-max` thread
//! never delays VBAN or the NDI submit (it only coalesces).
//! Wired via `#[cfg(test)] #[path = "program_output_tests_max.rs"] mod tests_max;`.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sp_core::genlock::{GENLOCK_GRID_FPS, floor_boundary_100ns, strict_next_boundary_100ns};
use sp_ndi::test_util::MockNdiBackend;
use sp_ndi::{AudioFrame, NdiSender};

use super::ProgramOutput;
use crate::playback::frame_buf::SharedFrame;
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
        video: SharedFrame::new(vec![90u8; width as usize * 3]),
        audio: vec![AudioFrame {
            data: vec![0.0; 3200],
            channels: 2,
            sample_rate: 48_000,
            timecode_100ns: None,
        }],
        video_tc_100ns: stamp,
        audio_tc_100ns: stamp,
        live: true,
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
        Some(MaxNext::Job(job)) => job,
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
