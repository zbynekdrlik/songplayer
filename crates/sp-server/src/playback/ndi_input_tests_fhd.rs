//! #223: the NDI input "OBS manuál" reaches `SP-program` as the 1920×1080
//! canvas. Its capture is a `ProgramJob::Source` at the source's own size
//! (4×2 here), like any playlist's pair, and the program sender fits it into
//! the canvas (split from `ndi_input_tests.rs` for the 1000-line cap; shares
//! its helpers as a child module).

use std::sync::Arc;

use super::*;
use crate::playback::program_bus::{PROGRAM_NDI_NAME, ProgramJob};
use crate::playback::program_output::ProgramOutput;
use sp_ndi::NdiSender;
use sp_ndi::test_util::MockNdiBackend;

#[test]
fn the_inputs_picture_reaches_sp_program_as_the_1920x1080_canvas() {
    let mut rig = rig(source_frames(1, 30), vec![Some(0)]);
    let mut jobs = rig.run(1);
    assert_eq!(jobs.len(), 1, "one pair on b(1)");
    let job = jobs.remove(0);
    assert_eq!(
        (job.width, job.height, job.stride),
        (4, 2, 4),
        "the input offers its source's own size"
    );
    let source = job.video.clone();
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), PROGRAM_NDI_NAME, false, false)
        .expect("mock sender");
    let mut out = ProgramOutput::fhd(sender);
    out.submit(ProgramJob::Source(job));
    let sends: Vec<String> = backend
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("send_video_async("))
        .collect();
    assert_eq!(
        sends,
        vec!["send_video_async(42,NV12,1920x1080,stride=1920,30/1)".to_string()],
        "the canvas, not the input's 4×2"
    );
    let (ptr, len) = backend.last_async_video_slice().expect("a picture");
    assert_eq!(len, 3_110_400, "1920 × 1080 × 3 / 2");
    assert_ne!(
        ptr,
        source.as_ptr() as usize,
        "fitted into a buffer of its own"
    );
}
