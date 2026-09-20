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

/// Karaoke playback mode (#14). Controls how the separated vocal / instrumental
/// stems are mixed into the NDI audio output. `FullMix` (default) plays the
/// original `{id}_audio.flac` unchanged — the current behaviour and the safe
/// fallback whenever a song's stems are missing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KaraokeMode {
    /// Original mix, stems untouched. Plays `{id}_audio.flac`.
    #[default]
    FullMix,
    /// Vocals attenuated to the user vocal-gain, instrumental at 100 %
    /// (practice / sing-along).
    KaraokeLow,
    /// Vocals only (vocal training).
    VocalsOnly,
    /// Instrumental only, zero vocals (true karaoke).
    InstrumentalOnly,
}

impl KaraokeMode {
    /// Stable string for storage (`settings` value) and the JSON API.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FullMix => "full_mix",
            Self::KaraokeLow => "karaoke_low",
            Self::VocalsOnly => "vocals_only",
            Self::InstrumentalOnly => "instrumental_only",
        }
    }

    /// Parse from a string, tolerating dashes and a few aliases; anything
    /// unrecognised falls back to the safe `FullMix`.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "karaoke_low" | "karaoke" | "low" => Self::KaraokeLow,
            "vocals_only" | "vocals" | "vocal" => Self::VocalsOnly,
            "instrumental_only" | "instrumental" | "instr" => Self::InstrumentalOnly,
            // "full_mix" | "full" | "" | anything else
            _ => Self::FullMix,
        }
    }

    /// Whether playback needs the separated stems (i.e. non-`FullMix`).
    /// `FullMix` plays the original mix file and never opens a stem.
    pub fn needs_stems(&self) -> bool {
        !matches!(self, Self::FullMix)
    }

    /// Encode as a `u8` for the shared `Arc<AtomicU8>` the engine hands each
    /// playback pipeline (mirrors the `burn_on` atomic seam).
    pub fn as_u8(&self) -> u8 {
        match self {
            Self::FullMix => 0,
            Self::KaraokeLow => 1,
            Self::VocalsOnly => 2,
            Self::InstrumentalOnly => 3,
        }
    }

    /// Decode from the shared atomic; any unknown value is the safe `FullMix`.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::KaraokeLow,
            2 => Self::VocalsOnly,
            3 => Self::InstrumentalOnly,
            _ => Self::FullMix,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn karaoke_mode_default_is_full_mix() {
        assert_eq!(KaraokeMode::default(), KaraokeMode::FullMix);
        assert!(!KaraokeMode::default().needs_stems());
    }

    #[test]
    fn karaoke_mode_str_round_trips() {
        for m in [
            KaraokeMode::FullMix,
            KaraokeMode::KaraokeLow,
            KaraokeMode::VocalsOnly,
            KaraokeMode::InstrumentalOnly,
        ] {
            assert_eq!(KaraokeMode::from_str_lossy(m.as_str()), m);
        }
    }

    #[test]
    fn karaoke_mode_as_str_values_are_stable() {
        assert_eq!(KaraokeMode::FullMix.as_str(), "full_mix");
        assert_eq!(KaraokeMode::KaraokeLow.as_str(), "karaoke_low");
        assert_eq!(KaraokeMode::VocalsOnly.as_str(), "vocals_only");
        assert_eq!(KaraokeMode::InstrumentalOnly.as_str(), "instrumental_only");
    }

    #[test]
    fn karaoke_mode_from_str_lossy_aliases_and_fallback() {
        assert_eq!(
            KaraokeMode::from_str_lossy("KARAOKE-LOW"),
            KaraokeMode::KaraokeLow
        );
        assert_eq!(
            KaraokeMode::from_str_lossy("vocals"),
            KaraokeMode::VocalsOnly
        );
        assert_eq!(
            KaraokeMode::from_str_lossy("instr"),
            KaraokeMode::InstrumentalOnly
        );
        assert_eq!(
            KaraokeMode::from_str_lossy("  full  "),
            KaraokeMode::FullMix
        );
        assert_eq!(KaraokeMode::from_str_lossy("garbage"), KaraokeMode::FullMix);
        assert_eq!(KaraokeMode::from_str_lossy(""), KaraokeMode::FullMix);
    }

    #[test]
    fn needs_stems_only_for_non_full_mix() {
        assert!(!KaraokeMode::FullMix.needs_stems());
        assert!(KaraokeMode::KaraokeLow.needs_stems());
        assert!(KaraokeMode::VocalsOnly.needs_stems());
        assert!(KaraokeMode::InstrumentalOnly.needs_stems());
    }

    #[test]
    fn karaoke_mode_u8_round_trips() {
        for m in [
            KaraokeMode::FullMix,
            KaraokeMode::KaraokeLow,
            KaraokeMode::VocalsOnly,
            KaraokeMode::InstrumentalOnly,
        ] {
            assert_eq!(KaraokeMode::from_u8(m.as_u8()), m);
        }
        // Unknown atomic value is the safe FullMix.
        assert_eq!(KaraokeMode::from_u8(99), KaraokeMode::FullMix);
    }

    #[test]
    fn karaoke_mode_serde_is_snake_case() {
        let json = serde_json::to_string(&KaraokeMode::KaraokeLow).unwrap();
        assert_eq!(json, "\"karaoke_low\"");
        let back: KaraokeMode = serde_json::from_str("\"instrumental_only\"").unwrap();
        assert_eq!(back, KaraokeMode::InstrumentalOnly);
    }
}
