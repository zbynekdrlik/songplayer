//! #212 NDI input: the converted NV12 picture's `frame_pool` sizing (split
//! from `ndi_input_tests.rs` for the 1000-line cap; shares its helpers as a
//! child module).

use super::*;

#[test]
fn the_converted_picture_is_a_pooled_buffer_of_exactly_the_nv12_size() {
    // 50×34 NV12 = 2550 bytes, a size class no other test uses: once every
    // holder is gone, the buffer is recycled into `frame_pool` under exactly
    // that capacity (a wrongly sized take would grow into another class).
    const CAP: usize = 50 * 34 * 3 / 2;
    let frame = MockVideoFrame {
        xres: 50,
        yres: 34,
        four_cc: FOURCC_UYVY,
        line_stride: 100,
        frame_rate_n: 30,
        frame_rate_d: 1,
        frame_format_type: FRAME_FORMAT_TYPE_PROGRESSIVE,
        timecode: 0,
        data: vec![77; 100 * 34],
    };
    let before = sp_decoder::frame_pool::pool_len(CAP);
    let mut rig = rig(vec![frame], vec![Some(0)]);
    let jobs = rig.run(1);
    assert_eq!(jobs[0].video.len(), CAP);
    assert_eq!(
        (jobs[0].width, jobs[0].height, jobs[0].stride),
        (50, 34, 50)
    );
    drop(jobs);
    drop(rig); // the input's own reference to the converted frame
    assert_eq!(sp_decoder::frame_pool::pool_len(CAP), before + 1);
}

#[test]
fn the_conversion_takes_a_pooled_buffer_of_exactly_the_nv12_size() {
    // The mutation gate (run 36258274374, shard 21/24) MISSED `/` -> `%` in
    // the `frame_pool::take(w * h * 3 / 2)` size: `take(0)` still converts
    // correctly (the buffer grows to exactly the NV12 size) but never REUSES
    // a pooled buffer. 52x36 NV12 = 2808 bytes, a size class no other test
    // uses: a buffer parked there must be the one the conversion takes.
    const CAP: usize = 52 * 36 * 3 / 2;
    sp_decoder::frame_pool::recycle(Vec::with_capacity(CAP));
    let parked = sp_decoder::frame_pool::pool_len(CAP);
    assert!(parked >= 1, "the parked buffer is in its class");
    let frame = MockVideoFrame {
        xres: 52,
        yres: 36,
        four_cc: FOURCC_UYVY,
        line_stride: 104,
        frame_rate_n: 30,
        frame_rate_d: 1,
        frame_format_type: FRAME_FORMAT_TYPE_PROGRESSIVE,
        timecode: 0,
        data: vec![77; 104 * 36],
    };
    let mut rig = rig(vec![frame], vec![Some(0)]);
    let jobs = rig.run(1);
    assert_eq!(jobs[0].video.len(), CAP);
    assert_eq!(
        sp_decoder::frame_pool::pool_len(CAP),
        parked - 1,
        "the conversion took the parked NV12-sized buffer"
    );
}
