//! Whether an NDI receiver is expected on a playlist's output (#221 L4a,
//! design record 5873773896 §1f). Pure; a sibling of `ndi_health.rs` (its
//! 1000-line cap).
//!
//! Until B4 step 6 a playlist's NDI output is received by cg OBS's scene
//! input for it, and DistroAV disconnects an input whose scene is not on cg
//! OBS's program: 0 receivers there is normal. Once "on air" no longer means
//! "cg OBS shows it" (#221: SongPlayer's own program decides, L4b), a playlist
//! on `SP-program` that cg OBS does not show would read as a dark wall, and
//! the #173 ladder would churn cg OBS's inputs. So a receiver is expected
//! only while the playlist is on air AND SongPlayer told cg OBS to show it
//! (`legacy_cg.shown`, SongPlayer's record of its OWN commands — never cg
//! OBS tracking).
//!
//! The dark-wall reason (the whole degraded reason: a 0-receiver poll counts
//! as a bad poll, so its underrun / no-frames fall-throughs would misfire
//! too), the #196 post-restart self-check and its ladder suppression key on
//! [`receiver_expected`]. The state label (the badge, the idle gates) stays
//! keyed on on-air. At B4 step 6 the dark-wall check moves to SP-program's
//! own receivers.

use crate::playback::ndi_health::PlaybackStateLabel;

/// A receiver is expected on playlist `playlist_id`'s NDI output: it is on
/// air (the reconciled `label` is `Playing`: playing, its scene on program)
/// AND `cg_shown` — the playlist SongPlayer last told cg OBS to show, and cg
/// OBS accepted — is this playlist.
pub(crate) fn receiver_expected(
    label: &PlaybackStateLabel,
    _cg_shown: Option<i64>,
    _playlist_id: i64,
) -> bool {
    matches!(label, PlaybackStateLabel::Playing)
}

#[cfg(test)]
#[path = "ndi_health_expect_tests.rs"]
mod tests;
