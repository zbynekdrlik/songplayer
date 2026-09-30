//! #224 part 2 (review round 1): the SDK-clocked audio emitter (#192, pacing
//! OFF) is an NDI wire edge too. Its emit thread's `WallClock::system()`
//! reads the internal TIMELINE (UTC − D(K)), so each block's grid stamp goes
//! on the fleet labels at the send: `t + D(K_F)`, unfloored (the emitter's
//! own grid is not the 30 fps UTC grid). Before, a pipeline built after a
//! date step stamped its audio D(K) away from its video (1.5 s after the
//! nightly step). Wired via `#[cfg(test)] #[path =
//! "audio_emitter_tests_regrid.rs"]` in `audio_emitter.rs`.

use std::sync::Arc;

use super::*;
use crate::playback::fleet_shift::{FleetShift, shift_100ns};
use sp_ndi::NdiSender;
use sp_ndi::test_util::MockNdiBackend;

fn mock_sink() -> (Arc<MockNdiBackend>, AudioSink<MockNdiBackend>) {
    let backend = Arc::new(MockNdiBackend::new());
    let sender = NdiSender::new_with_clocking(backend.clone(), "AE", false, false).unwrap();
    let sink = sender.audio_sink();
    // The sink holds its own Arc<backend> + handle; leak the sender so its
    // Drop (a mock send_destroy) records nothing mid-test.
    std::mem::forget(sender);
    (backend, sink)
}

#[test]
fn each_blocks_stamp_goes_on_the_fleet_labels_at_the_send() {
    // K_F = 7 after +260.3 ms, −1 after −19.8 ms, 0 with no step.
    for (step, k) in [(0i64, 0i64), (2_603_000, 7), (-198_000, -1)] {
        let fleet = Arc::new(FleetShift::default());
        if step != 0 {
            let _ = fleet.follow(0, step);
        }
        assert_eq!(fleet.slots(), k, "{step}");
        let shared = new_shared_emitter_on(fleet);
        let (backend, sink) = mock_sink();
        push_blocking(&shared, &vec![0.5f32; EMIT_SAMPLES_PER_BLOCK * 2], 2);
        // A timeline reading off the 30 fps grid: the first tick anchors
        // the emitter's grid on it, so the block's grid stamp is `t`.
        let t = 17_900_000_000_000_123i64;
        let first = emit_one_block(&shared, &sink, t);
        assert_eq!(first.timecode_100ns, t, "{step}: the internal grid stamp");
        let second = emit_one_block(&shared, &sink, t + 333_333);
        assert_eq!(
            backend.audio_timecodes(),
            vec![t + shift_100ns(k), second.timecode_100ns + shift_100ns(k)],
            "{step}: the fleet label t + D(K_F), unfloored"
        );
    }
}
