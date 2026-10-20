//! #279 P2: ring frames (`Picture::Gpu`, P3a's hardware decode) on MAX's
//! side. `PictureIds` names a ring frame by its OWN id (process-wide, never
//! reused) and never spends a counter id on it; a fade of a CPU picture and
//! a ring frame keeps both; and the worker hands the compositor the frame
//! itself (`Side::Gpu`: no upload). On `sp_gpu::FakeRing` and the fake GPU.
//! Wired as a child of `program_max_worker_tests.rs`.

use std::time::{Duration, Instant};

use sp_gpu::{Composition, FakeRing, GpuFrame, Side};

use super::{Drawn, FakeGpu, Pic, PictureIds, ids, picture, plain};
use crate::playback::frame_buf::{BlackNv12, SharedFrame};
use crate::playback::program_max::{GpuStatus, MaxJob, MaxOut, MaxPicture};
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
    // #279 P3a: the `gpu` block: one ring slot after the first in 100 ms.
    assert_eq!(
        max.gpu_status(),
        GpuStatus {
            gpu_sides_per_s: 10.0,
            ..GpuStatus::default()
        }
    );
    worker.serve(&plain(3, &picture(4, 2)), t0 + Duration::from_millis(200));
    let gpu = max.gpu_status();
    assert_eq!(
        (gpu.gpu_sides_per_s, gpu.cpu_sides_per_s),
        (5.0, 5.0),
        "one ring slot and one CPU picture after the first in 200 ms"
    );
}

/// #279 P3a: the paced standby black (a pipeline's idle, pre-roll or fill
/// picture, `frame_buf::BlackNv12`) is composed as `Composition::Black`: no
/// upload and no id; a fade's black side is a missing side; the same bytes
/// in a frame that is no standby black stay a picture.
#[test]
fn a_canvas_black_picture_is_composed_as_black() {
    let mut cache = BlackNv12::default();
    let black = MaxPicture {
        width: 4,
        height: 2,
        stride: 4,
        video: cache.get(4, 2).into(),
    };
    let other = picture(4, 2);
    let mut labels = PictureIds::default();
    assert!(matches!(
        labels.composition(&plain(1, &black)),
        Composition::Black
    ));
    assert_eq!(
        ids(&labels.composition(&plain(2, &other))),
        [Some(1)],
        "the black spent no id"
    );
    let fade_in = MaxJob::Fade {
        stamp_100ns: 3,
        from: Some(black.clone()),
        to: Some(other.clone()),
        weight_q8: 64,
    };
    assert_eq!(ids(&labels.composition(&fade_in)), [None, Some(1)]);
    let fade_out = MaxJob::Fade {
        stamp_100ns: 4,
        from: Some(other.clone()),
        to: Some(black.clone()),
        weight_q8: 64,
    };
    assert_eq!(ids(&labels.composition(&fade_out)), [Some(1), None]);
    let unmarked = MaxPicture {
        video: SharedFrame::new(black.video.expect_cpu().to_vec()).into(),
        ..black.clone()
    };
    assert_eq!(
        ids(&labels.composition(&plain(5, &unmarked))),
        [Some(2)],
        "black bytes in another frame: a picture"
    );
}

/// #279 P3a: the worker composes a standby black as black: no CPU side, no
/// upload.
#[test]
fn the_worker_draws_a_canvas_black_with_no_upload() {
    let max = MaxOut::new();
    max.set_enabled(true);
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let mut cache = BlackNv12::default();
    let black = MaxPicture {
        width: 4,
        height: 2,
        stride: 4,
        video: cache.get(4, 2).into(),
    };
    let t0 = Instant::now();
    worker.serve(&plain(1, &black), t0);
    worker.serve(&plain(2, &black), t0 + Duration::from_millis(100));
    assert_eq!(gpu.log().drawn, [Drawn::Black, Drawn::Black]);
    assert_eq!(max.status().uploads_per_s, 0.0);
    assert_eq!(
        max.gpu_status(),
        GpuStatus::default(),
        "no side of either kind"
    );
}
