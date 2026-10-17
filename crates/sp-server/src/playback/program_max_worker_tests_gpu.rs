//! #279 P2: ring frames (`Picture::Gpu`, P3a's hardware decode) on MAX's
//! side. `PictureIds` names a ring frame by its OWN id (process-wide, never
//! reused) and never spends a counter id on it; a fade of a CPU picture and
//! a ring frame keeps both; and the worker hands the compositor the frame
//! itself (`Side::Gpu`: no upload). On `sp_gpu::FakeRing` and the fake GPU.
//! Wired as a child of `program_max_worker_tests.rs`.

use std::time::{Duration, Instant};

use sp_gpu::{Composition, FakeRing, GpuFrame, Side};

use super::{Drawn, FakeGpu, Pic, PictureIds, ids, picture, plain};
use crate::playback::program_max::{MaxJob, MaxOut, MaxPicture};
use crate::playback::program_max_worker::MaxWorker;

/// A 4×2 picture in a ring slot of `frame`.
fn ring_picture(frame: &GpuFrame) -> MaxPicture {
    MaxPicture {
        width: 4,
        height: 2,
        stride: 4,
        video: frame.clone().into(),
    }
}

#[test]
fn a_ring_frame_is_labelled_by_its_own_id_and_spends_no_counter_id() {
    let ring = FakeRing::new(2);
    let frame = ring.frame(4, 2, Duration::ZERO).expect("a free slot");
    let (cpu, gpu) = (picture(4, 2), ring_picture(&frame));
    let mut labels = PictureIds::default();
    assert_eq!(ids(&labels.composition(&plain(1, &cpu))), [Some(1)]);
    let job = plain(2, &gpu);
    let Composition::Picture(Side::Gpu(drawn)) = labels.composition(&job) else {
        panic!("a ring frame is drawn in place, never uploaded");
    };
    assert!(drawn.ptr_eq(&frame), "the frame itself");
    assert_eq!(drawn.id(), frame.id(), "named by its own id");
    assert_eq!(
        ids(&labels.composition(&plain(3, &picture(4, 2)))),
        [Some(2)],
        "the ring frame took no counter id: the next new picture is 2"
    );
    let fade = MaxJob::Fade {
        stamp_100ns: 4,
        from: Some(cpu.clone()),
        to: Some(gpu.clone()),
        weight_q8: 64,
    };
    assert_eq!(
        ids(&labels.composition(&fade)),
        [Some(3), Some(frame.id())],
        "a CPU picture two boundaries back is not held: a new id; the ring side its own"
    );
}

#[test]
fn the_worker_hands_the_compositor_the_ring_frame_itself() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let ring = FakeRing::new(2);
    let a = ring.frame(4, 2, Duration::ZERO).expect("a free slot");
    let b = ring.frame(4, 2, Duration::ZERO).expect("a free slot");
    // Two new pictures 100 ms apart: two CPU pictures read 10 uploads/s
    // (`program_max_worker_tests_upload.rs`); ring frames read none.
    let t0 = Instant::now();
    worker.serve(&plain(1, &ring_picture(&a)), t0);
    worker.serve(
        &plain(2, &ring_picture(&b)),
        t0 + Duration::from_millis(100),
    );
    let side = |frame: &GpuFrame| {
        Drawn::Picture(Pic {
            id: frame.id(),
            width: 4,
            height: 2,
            stride: 0,
            len: 0,
            gpu: true,
        })
    };
    assert_eq!(gpu.log().drawn, [side(&a), side(&b)]);
    let status = max.status();
    assert_eq!(status.submitted, 2, "both boundaries went out");
    assert_eq!(
        (status.uploads_per_s, status.upload_mb_s),
        (0.0, 0.0),
        "a ring frame is drawn in place: nothing uploaded"
    );
}
