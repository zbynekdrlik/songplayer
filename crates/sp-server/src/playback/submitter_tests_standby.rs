//! #147 standby same-path (design comment 5841796900): the outer loop's standby
//! black and the cached paced NV12 black on [`FrameSubmitter`]. Wired via
//! `#[cfg(test)] #[path = "submitter_tests_standby.rs"] mod submitter_tests_standby;`.

use super::*;
use crate::playback::wallclock::WallClock;
use sp_ndi::test_util::MockNdiBackend;
use std::sync::Arc;

fn mock(name: &str, clock_video: bool) -> (Arc<MockNdiBackend>, FrameSubmitter<MockNdiBackend>) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), name, clock_video, false).unwrap();
    // Deliberately off-grid, so an on-grid stamp would differ from SYNTHESIZE.
    let sub = FrameSubmitter::new_with_wallclock(sender, 30, 1, WallClock::fixed(123_456_789));
    (backend, sub)
}

#[test]
fn standby_black_is_the_legacy_sync_bgra_and_nothing_when_paced() {
    // SDK-clocked (legacy): the outer loop's standby black is the unchanged
    // synchronous BGRA frame, SYNTHESIZE-stamped, full 1920x1080x4 bytes.
    let (legacy_backend, mut legacy) = mock("BL", true);
    legacy.send_standby_black(1920, 1080);
    let calls = legacy_backend.calls();
    assert!(
        calls
            .iter()
            .any(|c| c == "send_video(42,BGRA,1920x1080,stride=7680,30/1)"),
        "legacy standby is the sync BGRA black: {calls:#?}"
    );
    assert_eq!(
        legacy_backend.video_timecodes(),
        vec![sp_ndi::NDI_SEND_TIMECODE_SYNTHESIZE],
        "the legacy SDK-clocked standby black stays SYNTHESIZE"
    );
    assert_eq!(legacy_backend.last_sync_video_len(), Some(1920 * 1080 * 4));

    // Paced: NOTHING leaves the sender — no sync `send_video`, no BGRA, not even
    // a flush. The paced idle fill owns standby (NV12, async, on-grid, with a
    // silent block), so the paced output never carries a sync BGRA frame (#147).
    let (paced_backend, mut paced) = mock("BP", false);
    paced.set_paced(true);
    paced.send_standby_black(1920, 1080);
    assert_eq!(
        paced_backend.calls(),
        vec!["send_create_with_clocking(BP,false,false)".to_string()],
        "a paced standby black sends nothing"
    );
    assert!(paced_backend.video_timecodes().is_empty());
    assert_eq!(paced_backend.last_sync_video_len(), None);
}

#[test]
fn standby_black_nv12_is_neutral_black_nv12_of_the_exact_size() {
    let (_backend, mut sub) = mock("NV", false);
    let black = sub.standby_black_nv12(4, 2);
    // NV12 = a W×H luma plane + a W×H/2 interleaved chroma plane.
    assert_eq!(black.len(), 4 * 2 * 3 / 2, "exact NV12 size");
    assert!(
        black[..8].iter().all(|&y| y == 16),
        "luma is studio black 16: {:?}",
        &black[..]
    );
    assert!(
        black[8..].iter().all(|&uv| uv == 128),
        "chroma is neutral 128: {:?}",
        &black[..]
    );
}

#[test]
fn standby_black_nv12_is_built_once_and_rebuilt_only_on_a_size_change() {
    let (_backend, mut sub) = mock("NC", false);
    let first = sub.standby_black_nv12(4, 2);
    let again = sub.standby_black_nv12(4, 2);
    assert!(
        again.ptr_eq(&first),
        "the same size hands out the SAME allocation (built once per pipeline)"
    );

    // A different WIDTH alone rebuilds it (and a real `&&` on the size key: a
    // width-only change must not reuse the cached frame).
    let wider = sub.standby_black_nv12(8, 2);
    assert!(!wider.ptr_eq(&first), "a new width rebuilds the black");
    assert_eq!(wider.len(), 8 * 2 * 3 / 2);

    // A different HEIGHT alone rebuilds it too.
    let taller = sub.standby_black_nv12(8, 4);
    assert!(!taller.ptr_eq(&wider), "a new height rebuilds the black");
    assert_eq!(taller.len(), 8 * 4 * 3 / 2);

    // And the rebuilt one is the new cached frame.
    let cached = sub.standby_black_nv12(8, 4);
    assert!(cached.ptr_eq(&taller), "the rebuilt black is cached");
}
