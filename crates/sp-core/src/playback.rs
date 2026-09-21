//! Playback mode and state enums.

use serde::{Deserialize, Serialize};

/// How the player advances through videos.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackMode {
    #[default]
    Continuous,
    Single,
    Loop,
}

impl PlaybackMode {
    /// Returns a stable string representation for storage/display.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Continuous => "continuous",
            Self::Single => "single",
            Self::Loop => "loop",
        }
    }

    /// Parses a string into a `PlaybackMode`, falling back to `Continuous`
    /// for any unrecognised input.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "continuous" => Self::Continuous,
            "single" => Self::Single,
            "loop" => Self::Loop,
            _ => Self::Continuous,
        }
    }
}

/// High-level playback state of a playlist player.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackState {
    #[default]
    Idle,
    WaitingForScene,
    Playing,
}

/// The pipeline's OWN transport state, INDEPENDENT of whether its NDI output is
/// on OBS program (#201). Orthogonal to [`PlaybackState`]: `PlaybackState` folds
/// the on/off-program fact in (a decoding pipeline off program is reported as
/// `WaitingForScene` so the wall-health / selector logic stays correct), while
/// `TransportState` answers only "is the pipeline decoding right now?".
///
/// The shared Player reads `transport == Playing` for its play/pause label so a
/// dub prepared OFF program on the Dabing page reads `⏸ Pauza` while it plays,
/// and shows on/off-program only in the badge (from `ndi_health`). `serde(default)`
/// (=`Idle`) so older/mock payloads that omit it still decode.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportState {
    /// The pipeline is actively decoding a video (on OR off program).
    Playing,
    /// The pipeline has content but is not decoding (paused / black-framed /
    /// awaiting its scene).
    Paused,
    /// No video is loaded.
    #[default]
    Idle,
}

// #184 round G: the `KaraokeMode` enum was deleted. A karaoke MODE is no longer a
// live setting — the ONE mixer is three independent faders (`sp_core::mixer_model::
// MixFaders`) whose preset ids (`full_mix` / `karaoke_low` / `vocals_only` /
// `instrumental_only`) are plain snapshot strings in `mixer_model`, not an enum.
