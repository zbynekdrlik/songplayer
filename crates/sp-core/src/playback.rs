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
