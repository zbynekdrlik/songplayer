//! Mutation-killing unit tests for [`FrameSubmitter`] (#153, PR #153).
//!
//! Kills:
//!  * `submit_frame_at_boundary`'s frame-counter increment (`+= 1` → `*= 1`):
//!    a `*= 1` starting from 0 stays 0, so ONE submit distinguishes it (both the
//!    `frames_submitted_total` and `frames_in_window` counters are covered).
//!  * `submit_audio_tail` body-replacement (`-> ()`): the mutant sends no audio,
//!    so the mock records no audio timecode.
//!
//! Wired via `#[cfg(test)] #[path = "submitter_tests_mutants.rs"]` at the bottom
//! of `submitter.rs`; `super::*` resolves to the `submitter` module under test
//! (bringing in `AudioFrame`, `NdiSender`, `FrameSubmitter`).

use super::*;
use sp_ndi::test_util::MockNdiBackend;
use std::sync::Arc;

fn mk_audio(interleaved: Vec<f32>, channels: u32) -> AudioFrame {
    AudioFrame {
        data: interleaved,
        channels,
        sample_rate: 48_000,
        timecode_100ns: None,
    }
}

#[test]
fn submit_frame_at_boundary_bumps_frames_submitted_total_by_one() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "MB1", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);
    let data = vec![0u8; 12];

    assert_eq!(sub.frames_submitted_total(), 0);
    sub.submit_frame_at_boundary(4, 2, 4, &data, &[], 333_333, 333_333);
    // `+= 1` → 1; the `*= 1` mutant leaves 0 * 1 = 0.
    assert_eq!(sub.frames_submitted_total(), 1);
    sub.submit_frame_at_boundary(4, 2, 4, &data, &[], 666_666, 666_666);
    sub.submit_frame_at_boundary(4, 2, 4, &data, &[], 999_999, 999_999);
    // Three real submits → 3; the `*= 1` mutant stays pinned at 0.
    assert_eq!(sub.frames_submitted_total(), 3);
}

#[test]
fn submit_frame_at_boundary_bumps_frames_in_window_by_one() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "MB2", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);
    let data = vec![0u8; 12];

    sub.submit_frame_at_boundary(4, 2, 4, &data, &[], 333_333, 333_333);
    // `drain_window` snapshots `frames_in_window` then resets it. `+= 1` → 1;
    // the `*= 1` mutant leaves it at 0.
    let stats = sub.drain_window();
    assert_eq!(stats.frames_in_window, 1);
}

#[test]
fn submit_audio_tail_sends_the_audio_stamped_with_the_given_timecode() {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "MB3", false, false).unwrap();
    let mut sub = FrameSubmitter::new(sender, 30, 1);
    let tc: i64 = 4_242_424;

    sub.submit_audio_tail(&[mk_audio(vec![0.1, 0.2, 0.3, 0.4], 2)], tc);

    // The `-> ()` body-replacement mutant sends nothing, so `audio_timecodes()`
    // would be empty. Correct code sends one audio chunk stamped with `tc`.
    assert_eq!(
        backend.audio_timecodes(),
        vec![tc],
        "submit_audio_tail must send the audio chunk stamped with the given timecode"
    );
    let calls = backend.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("send_audio(42,sr=48000,ch=2,spc=2)")),
        "exactly the audio-tail chunk must reach the sender: {calls:#?}"
    );
    // Audio-only tail: no video send, no frame-counter bump.
    assert!(
        !calls.iter().any(|c| c.starts_with("send_video")),
        "audio tail must not send any video frame: {calls:#?}"
    );
    assert_eq!(
        sub.frames_submitted_total(),
        0,
        "audio tail is not a frame submission"
    );
}
