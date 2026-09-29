//! #221 L4a: `receiver_expected` (pure). The engine-level effect on the
//! dark-wall reason, the ladder and the #196 self-check is pinned in
//! `ndi_health_tests_expect.rs`.
//! Wired via `#[cfg(test)] #[path = "ndi_health_expect_tests.rs"] mod tests;`.

use super::receiver_expected;
use crate::playback::ndi_health::PlaybackStateLabel;

#[test]
fn a_receiver_is_expected_only_on_air_and_where_cg_obs_was_told() {
    let playing = PlaybackStateLabel::Playing;
    assert!(receiver_expected(&playing, Some(7), 7));
    assert!(
        !receiver_expected(&playing, Some(3), 7),
        "cg OBS shows another playlist"
    );
    assert!(
        !receiver_expected(&playing, None, 7),
        "cg OBS shows a manual scene, or nothing was told"
    );
    for off_air in [
        PlaybackStateLabel::Paused,
        PlaybackStateLabel::WaitingForScene,
        PlaybackStateLabel::Idle,
    ] {
        assert!(
            !receiver_expected(&off_air, Some(7), 7),
            "{off_air:?} is not on air"
        );
    }
}
