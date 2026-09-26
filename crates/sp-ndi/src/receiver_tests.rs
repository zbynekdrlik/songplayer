//! #212: `NdiFrameSync` over `MockNdiReceiveBackend` — the RAII order, the
//! capture guards, the exact bytes / samples, and the mock's own recorders
//! (the per-package mutation gate needs them asserted in THIS crate).

use std::sync::Arc;

use super::*;
use crate::receive::{FOURCC_UYVY, NdiReceiveBackend};
use crate::receive_mock::{MockNdiReceiveBackend, MockVideoFrame};

fn connected() -> (Arc<MockNdiReceiveBackend>, NdiFrameSync) {
    let mock = Arc::new(MockNdiReceiveBackend::default());
    let sync = NdiFrameSync::connect(mock.clone(), "CG-OBS (manual)", "SP-input").unwrap();
    (mock, sync)
}

fn frame(tc: i64, data: Vec<u8>) -> MockVideoFrame {
    MockVideoFrame {
        xres: 2,
        yres: 2,
        four_cc: FOURCC_UYVY,
        line_stride: 4,
        frame_rate_n: 30,
        frame_rate_d: 1,
        timecode: tc,
        data,
    }
}

#[test]
fn connect_creates_the_receiver_then_the_framesync_and_drop_reverses_it() {
    let (mock, sync) = connected();
    assert_eq!(sync.source(), "CG-OBS (manual)");
    assert_eq!(
        mock.calls(),
        vec![
            "recv_create(CG-OBS (manual),SP-input)",
            "framesync_create(1)"
        ]
    );
    drop(sync);
    assert_eq!(
        mock.calls()[2..],
        ["framesync_destroy(2)", "recv_destroy(1)"],
        "the FrameSync is destroyed before its receiver"
    );
}

#[test]
fn a_failed_framesync_destroys_the_receiver_it_was_made_for() {
    let mock = Arc::new(MockNdiReceiveBackend::default());
    mock.set_fail_create(false, true);
    let err = NdiFrameSync::connect(mock.clone(), "A (b)", "SP-input").err();
    assert!(matches!(err, Some(NdiError::ReceiveFailed(_))));
    assert_eq!(
        mock.calls(),
        vec![
            "recv_create(A (b),SP-input)",
            "framesync_create(1)",
            "recv_destroy(1)"
        ]
    );
}

#[test]
fn a_failed_receiver_creates_nothing_else() {
    let mock = Arc::new(MockNdiReceiveBackend::default());
    mock.set_fail_create(true, false);
    assert!(NdiFrameSync::connect(mock.clone(), "A (b)", "SP-input").is_err());
    assert_eq!(mock.calls(), vec!["recv_create(A (b),SP-input)"]);
}

#[test]
fn connections_and_queue_depth_read_the_backend() {
    let (mock, sync) = connected();
    assert_eq!(sync.connections(), 0);
    assert_eq!(sync.audio_queue_depth(), 0);
    mock.set_connections(1);
    mock.set_audio_queue_depth(3200);
    assert_eq!(sync.connections(), 1);
    assert_eq!(sync.audio_queue_depth(), 3200);
}

#[test]
fn a_video_capture_exposes_the_exact_plane_and_is_freed_on_drop() {
    let (mock, sync) = connected();
    mock.set_video_frames(vec![frame(700, vec![1, 2, 3, 4, 5, 6, 7, 8])]);
    mock.set_video_schedule(vec![Some(0)]);
    {
        let v = sync.capture_video();
        assert_eq!(mock.outstanding_video(), 1);
        assert_eq!(v.frame().timecode, 700);
        assert_eq!(v.frame().four_cc, FOURCC_UYVY);
        assert_eq!(v.first_plane(), Some(&[1u8, 2, 3, 4, 5, 6, 7, 8][..]));
    }
    assert_eq!(mock.outstanding_video(), 0, "the guard freed the frame");
    assert_eq!(
        mock.calls()[2..],
        ["framesync_capture_video(2)", "framesync_free_video(2)"]
    );
}

#[test]
fn the_all_zero_frame_has_no_plane_and_is_still_freed() {
    let (mock, sync) = connected();
    {
        let v = sync.capture_video();
        assert!(v.first_plane().is_none());
        assert_eq!(v.frame().xres, 0);
    }
    assert_eq!(mock.outstanding_video(), 0);
}

#[test]
fn a_degenerate_descriptor_has_no_plane() {
    let (mock, sync) = connected();
    let mut bad = [
        frame(1, vec![0; 8]),
        frame(2, vec![0; 8]),
        frame(3, vec![0; 8]),
    ];
    bad[0].xres = 0;
    bad[1].yres = 0;
    bad[2].line_stride = 0;
    mock.set_video_frames(bad.to_vec());
    mock.set_video_schedule(vec![Some(0), Some(1), Some(2)]);
    for _ in 0..3 {
        assert!(sync.capture_video().first_plane().is_none());
    }
    // A 1×1 frame with a 1-byte stride is the smallest valid one.
    let mut tiny = frame(4, vec![9]);
    tiny.xres = 1;
    tiny.yres = 1;
    tiny.line_stride = 1;
    mock.set_video_frames(vec![tiny]);
    mock.set_video_schedule(vec![Some(0)]);
    assert_eq!(sync.capture_video().first_plane(), Some(&[9u8][..]));
}

#[test]
fn the_schedule_repeats_its_last_entry_and_none_is_the_zero_frame() {
    let (mock, sync) = connected();
    mock.set_video_frames(vec![frame(10, vec![0; 8]), frame(20, vec![0; 8])]);
    mock.set_video_schedule(vec![None, Some(1), Some(0)]);
    let tcs: Vec<i64> = (0..4)
        .map(|_| sync.capture_video().frame().timecode)
        .collect();
    assert_eq!(tcs, vec![0, 20, 10, 10]);
    // The same scripted frame keeps the same pointer (a FrameSync repeat).
    let a = sync.capture_video().frame().p_data as usize;
    let b = sync.capture_video().frame().p_data as usize;
    assert_eq!(a, b);
    assert_ne!(a, 0);
}

#[test]
fn an_audio_capture_asks_for_the_exact_format_and_interleaves_the_planes() {
    let (mock, sync) = connected();
    // 2 planes of 3 samples, 4 floats apart (one padding float per plane).
    mock.set_audio(vec![0.1, 0.2, 0.3, 9.0, -0.1, -0.2, -0.3, 9.0], 2, 3, 16);
    {
        let a = sync.capture_audio(48_000, 2, 3);
        assert_eq!(mock.outstanding_audio(), 1);
        assert_eq!(a.frame().sample_rate, 48_000);
        assert_eq!(a.interleaved(2, 3), vec![0.1, -0.1, 0.2, -0.2, 0.3, -0.3]);
        // Asking for more samples / channels than the source has pads silence.
        assert_eq!(
            a.interleaved(3, 4),
            vec![
                0.1, -0.1, 0.0, 0.2, -0.2, 0.0, 0.3, -0.3, 0.0, 0.0, 0.0, 0.0
            ]
        );
    }
    assert_eq!(mock.outstanding_audio(), 0);
    assert_eq!(
        mock.calls()[2..],
        [
            "framesync_capture_audio(2,48000,2,3)",
            "framesync_free_audio(2)"
        ]
    );
}

#[test]
fn an_empty_audio_frame_interleaves_to_silence() {
    let (mock, sync) = connected();
    let a = sync.capture_audio(48_000, 2, 4);
    assert!(a.frame().p_data.is_null());
    assert_eq!(a.interleaved(2, 4), vec![0.0; 8]);
    drop(a);
    assert_eq!(mock.outstanding_audio(), 0);
}

#[test]
fn a_degenerate_audio_descriptor_interleaves_to_silence() {
    let (mock, sync) = connected();
    mock.set_audio(vec![0.5; 4], 0, 2, 8); // no channels
    assert_eq!(
        sync.capture_audio(48_000, 2, 2).interleaved(2, 2),
        vec![0.0; 4]
    );
    mock.set_audio(vec![0.5; 4], 2, 2, 0); // no stride
    assert_eq!(
        sync.capture_audio(48_000, 2, 2).interleaved(2, 2),
        vec![0.0; 4]
    );
    mock.set_audio(vec![0.5; 4], 2, -1, 8); // negative sample count
    assert_eq!(
        sync.capture_audio(48_000, 2, 2).interleaved(2, 2),
        vec![0.0; 4]
    );
    mock.set_audio(vec![0.5; 4], -2, 2, 8); // negative channel count
    assert_eq!(
        sync.capture_audio(48_000, 2, 2).interleaved(2, 2),
        vec![0.0; 4]
    );
    mock.set_audio(vec![0.5; 4], 2, 2, -8); // negative stride
    assert_eq!(
        sync.capture_audio(48_000, 2, 2).interleaved(2, 2),
        vec![0.0; 4]
    );
    // The same block with sane counts is read: the guards above are what
    // silenced it.
    mock.set_audio(vec![0.5, 0.25, -0.5, -0.25], 2, 2, 8);
    assert_eq!(
        sync.capture_audio(48_000, 2, 2).interleaved(2, 2),
        vec![0.5, -0.5, 0.25, -0.25]
    );
}

#[test]
fn interleave_planar_places_every_sample_exactly() {
    let planar = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    assert_eq!(
        interleave_planar(&planar, 3, 2, 3, 2, 3),
        vec![1.0, 4.0, 2.0, 5.0, 3.0, 6.0]
    );
    // Fewer source samples than requested: the tail is silence.
    assert_eq!(
        interleave_planar(&planar, 3, 2, 2, 2, 3),
        vec![1.0, 4.0, 2.0, 5.0, 0.0, 0.0]
    );
    // More source samples than requested: truncated.
    assert_eq!(
        interleave_planar(&planar, 3, 2, 3, 2, 2),
        vec![1.0, 4.0, 2.0, 5.0]
    );
    // A stride shorter than the claimed sample count is never read past.
    assert_eq!(
        interleave_planar(&planar, 2, 3, 3, 3, 2),
        vec![1.0, 3.0, 5.0, 2.0, 4.0, 6.0]
    );
    // A buffer too short for a plane leaves that channel silent.
    assert_eq!(
        interleave_planar(&planar[..4], 3, 2, 3, 2, 3),
        vec![1.0, 0.0, 2.0, 0.0, 3.0, 0.0]
    );
    // Mono source into stereo: the second channel is silence.
    assert_eq!(
        interleave_planar(&[7.0, 8.0], 2, 1, 2, 2, 2),
        vec![7.0, 0.0, 8.0, 0.0]
    );
}

#[test]
fn the_mock_finder_returns_the_configured_names() {
    let mock = MockNdiReceiveBackend::default();
    assert!(mock.find_source_names(250).is_empty());
    mock.set_sources(vec!["CG-OBS (manual)".into(), "CAM (1)".into()]);
    assert_eq!(
        mock.find_source_names(1000),
        vec!["CG-OBS (manual)".to_string(), "CAM (1)".to_string()]
    );
    assert_eq!(
        mock.calls(),
        vec!["find_source_names(250)", "find_source_names(1000)"]
    );
}
