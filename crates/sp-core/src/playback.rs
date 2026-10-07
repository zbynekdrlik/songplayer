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

    /// Parses a stored or requested mode (case-insensitive): `None` for
    /// anything but the three names [`as_str`](Self::as_str) writes. The one
    /// parser, so a caller that must not take an unknown value as the default
    /// (#225: the playlist row, the mode routes) can tell it apart.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "continuous" => Some(Self::Continuous),
            "single" => Some(Self::Single),
            "loop" => Some(Self::Loop),
            _ => None,
        }
    }

    /// Parses a string into a `PlaybackMode`, falling back to `Continuous`
    /// for any unrecognised input.
    pub fn from_str_lossy(s: &str) -> Self {
        Self::parse(s).unwrap_or_default()
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

/// The pipeline's OWN transport state, INDEPENDENT of whether its playlist is
/// on SongPlayer's program (#201; #221 L4b). Orthogonal to [`PlaybackState`]:
/// `PlaybackState` folds the on/off-program fact in (a decoding pipeline off
/// program is reported as `WaitingForScene` so the wall-health / selector
/// logic stays correct), while
/// `TransportState` answers only "is the pipeline decoding right now?".
///
/// The shared Player reads `transport == Playing` for its play/pause label so a
/// dub prepared OFF program on the Dabing page reads `⏸ Pauza` while it plays,
/// and shows on/off-program only in the badge (from the scene-aware `state`,
/// #225). `serde(default)` (=`Idle`) so older/mock payloads that omit it still
/// decode.
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

/// #229: the videos of a playlist that failed to open in a row, as
/// `GET /api/v1/ndi/health` reports it per playlist (`open_failures`; `null`
/// while none failed since the last song started). The Player's line about
/// it is `player_view::open_failures_line`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFailures {
    /// Failed opens in a row.
    pub count: u32,
    /// The last one's error, as the pipeline reported it.
    pub last_error: String,
    /// When the next attempt is due (UTC ms since the epoch). `None` = no
    /// wait is pending: the next song is tried at once, an attempt is under
    /// way, or the playlist was cut off program or paused.
    pub retry_at_ms: Option<i64>,
    /// The wait left until `retry_at_ms` when the row was READ, on the
    /// server's own clock (0 once due): what a dashboard counts down, since
    /// the browser's clock on another machine can be off.
    #[serde(default)]
    pub retry_in_ms: Option<u64>,
}

impl OpenFailures {
    /// The row as read at `now_ms` (UTC ms, the server's clock): its
    /// `retry_in_ms` is the wait left until `retry_at_ms`, 0 once due.
    pub fn read_at(self, now_ms: i64) -> Self {
        let retry_in_ms = self
            .retry_at_ms
            .map(|at_ms| u64::try_from(at_ms.saturating_sub(now_ms)).unwrap_or(0));
        Self {
            retry_in_ms,
            ..self
        }
    }
}

// #184 round G: the `KaraokeMode` enum was deleted. A karaoke MODE is no longer a
// live setting — the ONE mixer is three independent faders (`sp_core::mixer_model::
// MixFaders`) whose preset ids (`full_mix` / `karaoke_low` / `vocals_only` /
// `instrumental_only`) are plain snapshot strings in `mixer_model`, not an enum.

#[cfg(test)]
mod tests {
    use super::OpenFailures;

    fn failures(retry_at_ms: Option<i64>) -> OpenFailures {
        OpenFailures {
            count: 3,
            last_error: "x".into(),
            retry_at_ms,
            retry_in_ms: None,
        }
    }

    /// #229: the wait left is read on the server's own clock, 0 once due.
    #[test]
    fn a_row_read_now_tells_the_wait_left() {
        assert_eq!(
            failures(Some(6_500)).read_at(1_000).retry_in_ms,
            Some(5_500)
        );
        assert_eq!(failures(Some(1_000)).read_at(1_000).retry_in_ms, Some(0));
        assert_eq!(
            failures(Some(1_000)).read_at(9_000).retry_in_ms,
            Some(0),
            "past: 0, never negative"
        );
        assert_eq!(
            failures(None).read_at(1_000).retry_in_ms,
            None,
            "no retry pending"
        );
        let read = failures(Some(6_500)).read_at(1_000);
        assert_eq!(
            (read.count, read.last_error.as_str(), read.retry_at_ms),
            (3, "x", Some(6_500)),
            "the rest of the row is kept"
        );
    }
}
