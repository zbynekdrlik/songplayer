//! #279 P0: the `program-max` thread reports what each sent boundary's
//! compose uploaded (`max.upload_mb_s`, `max.uploads_per_s`), at the
//! instant it served the boundary, on the fake GPU. The fake's compose
//! uploads every picture of its composition ([`fake_stats`]: it keeps no
//! residency; sp-gpu's WARP pins prove the real compositor's), so a Black
//! boundary uploads nothing.
//! Wired via `#[cfg(test)] #[path = "program_max_worker_tests_upload.rs"] mod tests_upload;`.

use std::time::{Duration, Instant};

use sp_gpu::{ComposeStats, Composition, GpuError};

use super::MaxWorker;
use super::tests::{DRAW_US, FakeGpu, UPLOAD_US, picture, plain};
use super::tests_fhd::fhd_on;
use crate::playback::program_max::{MaxJob, MaxOut};

const MS: Duration = Duration::from_millis(1);

/// One 1920×1080 NV12 picture's planes: 1920×1080 luma + 960×540 U/V
/// pairs of two bytes.
const FHD_PICTURE_BYTES: u64 = 1920 * 1080 + 2 * 960 * 540;

/// The fake compositor's costs of one compose: every picture of
/// `composition` uploaded, its plane bytes by sp-gpu's own rule
/// (`Planes::upload_bytes`).
pub(super) fn fake_stats(composition: &Composition<'_>) -> ComposeStats {
    let pictures = composition.pictures();
    let upload_bytes = pictures
        .iter()
        .map(|(_, picture)| picture.planes().map_or(0, |planes| planes.upload_bytes()))
        .sum();
    ComposeStats {
        upload_us: UPLOAD_US,
        draw_us: DRAW_US,
        uploads: u32::try_from(pictures.len()).unwrap_or(u32::MAX),
        upload_bytes,
    }
}

#[test]
fn the_fake_uploads_each_picture_of_its_composition() {
    let (a, b) = (picture(1920, 1080), picture(4, 2));
    let fade = Composition::Fade {
        from: Some(a.nv12(1)),
        to: Some(b.nv12(2)),
        weight_q8: 64,
    };
    let stats = fake_stats(&fade);
    assert_eq!(
        (stats.uploads, stats.upload_bytes),
        (2, FHD_PICTURE_BYTES + 8 + 2 * 2)
    );
    let black = fake_stats(&Composition::Black);
    assert_eq!((black.uploads, black.upload_bytes), (0, 0));
}

/// Eleven boundaries 100 ms apart, each a new 1920×1080 picture: ten
/// uploads after the first in one second.
#[test]
fn each_sent_boundary_s_upload_is_counted_at_the_instant_it_was_served() {
    let max = MaxOut::new();
    let mut worker = MaxWorker::new(&max, FakeGpu::default());
    let t0 = Instant::now();
    for i in 0..=10u32 {
        let fresh = picture(1920, 1080);
        worker.serve(&plain(i64::from(i), &fresh), t0 + 100 * MS * i);
    }
    let status = max.status();
    assert_eq!(status.submitted, 11);
    assert_eq!(
        (status.upload_mb_s, status.uploads_per_s),
        (31.1, 10.0),
        "10 × 3 110 400 bytes in 1 s"
    );
}

/// A Black boundary uploads nothing, and still counts in the time.
#[test]
fn a_black_boundary_uploads_nothing() {
    let max = MaxOut::new();
    let mut worker = MaxWorker::new(&max, FakeGpu::default());
    let t0 = Instant::now();
    let (a, b) = (picture(1920, 1080), picture(1920, 1080));
    worker.serve(&plain(1, &a), t0);
    worker.serve(&MaxJob::Black { stamp_100ns: 2 }, t0 + 500 * MS);
    worker.serve(&plain(3, &b), t0 + 1_000 * MS);
    let status = max.status();
    assert_eq!(
        (status.upload_mb_s, status.uploads_per_s),
        (3.1, 1.0),
        "one picture after the first, in 1 s"
    );
}

/// A boundary that did not go out (its Spout frame lost) is not counted:
/// the rates cover the sent boundaries, like the `*_us_p99` windows.
#[test]
fn a_boundary_that_did_not_go_out_is_not_counted() {
    let max = MaxOut::new();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    worker.serve(&plain(1, &picture(1920, 1080)), t0);
    gpu.script().send.push_back(GpuError::Spout {
        call: "spout_sender_send",
        code: 3,
    });
    worker.serve(&plain(2, &picture(1920, 1080)), t0 + 1_000 * MS);
    worker.serve(&plain(3, &picture(1920, 1080)), t0 + 2_000 * MS);
    let status = max.status();
    assert_eq!((status.submitted, status.failed), (2, 1));
    assert_eq!(
        (status.upload_mb_s, status.uploads_per_s),
        (1.6, 0.5),
        "one picture after the first in 2 s (the lost boundary's not counted)"
    );
}

/// #251 step 2: one compose draws both targets from one upload, so the FHD
/// sender adds no upload: the same rates as MAX alone.
#[test]
fn the_fhd_sender_adds_no_upload() {
    let max = fhd_on();
    let gpu = FakeGpu::default();
    let mut worker = MaxWorker::new(&max, gpu.clone());
    let t0 = Instant::now();
    for i in 0..=10u32 {
        let fresh = picture(1920, 1080);
        worker.serve(&plain(i64::from(i), &fresh), t0 + 100 * MS * i);
    }
    assert!(gpu.log().fhd_sent >= 9, "the FHD sender sends too");
    let status = max.status();
    assert_eq!((status.upload_mb_s, status.uploads_per_s), (31.1, 10.0));
}
