//! Application configuration constants and setting keys.

/// Current application version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default API server port.
pub const DEFAULT_API_PORT: u16 = 8920;

// Setting key constants — used as keys in the settings table.
pub const SETTING_OBS_WEBSOCKET_URL: &str = "obs_websocket_url";
pub const SETTING_OBS_WEBSOCKET_PASSWORD: &str = "obs_websocket_password";
pub const SETTING_GEMINI_API_KEY: &str = "gemini_api_key";
pub const SETTING_GEMINI_MODEL: &str = "gemini_model";
pub const SETTING_CACHE_DIR: &str = "cache_dir";
pub const SETTING_MAX_RESOLUTION: &str = "max_resolution";
pub const SETTING_API_PORT: &str = "api_port";
/// #184 round H step 2: the dub output voice. `speaker` (the default,
/// [`DUB_VOICE_SPEAKER`]) = the model speaks in the speaker's OWN voice (no
/// `speech_config`, owner decision #184 5797691198); any other value pins that
/// Gemini prebuilt voice. Read per job by the dub worker, set from Nastavenia.
pub const SETTING_DUB_VOICE: &str = "dub_voice";
/// #184 round H step 2: the Gemini Live Translate model the dub session uses —
/// upgrading to a newer model is a setting change + one probe run, not a rebuild
/// (owner directive #184 5797708129). Read per job by the dub worker.
pub const SETTING_DUB_MODEL: &str = "dub_model";
/// #184 round G1: the ONE mixer console with TWO remembered fader triples,
/// selected by the KIND of the playing item — a SONG memory (`vokály` / `podklad`;
/// its `dabing` is unused) and a DUB memory (`vokály` / `podklad` / `dabing`).
/// Each fader is an f32 `0.0..=1.0` stored as its decimal string, persisted here
/// and restored at boot by `stems::control::MixControl`. Migration V28 derives the
/// song pair from round-G's single `mix_vokaly` / `mix_podklad` and seeds the dub
/// triple with the `Len dabing` default `(0, 1, 1)`.
pub const SETTING_MIX_SONG_VOKALY: &str = "mix_song_vokaly";
pub const SETTING_MIX_SONG_PODKLAD: &str = "mix_song_podklad";
pub const SETTING_MIX_DUB_VOKALY: &str = "mix_dub_vokaly";
pub const SETTING_MIX_DUB_PODKLAD: &str = "mix_dub_podklad";
pub const SETTING_MIX_DUB_DABING: &str = "mix_dub_dabing";

// Default values for settings that have sensible defaults.
pub const DEFAULT_OBS_WEBSOCKET_URL: &str = "ws://127.0.0.1:4455";
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-3.1-pro-preview";
pub const DEFAULT_CACHE_DIR: &str = "cache";
pub const DEFAULT_MAX_RESOLUTION: u32 = 1440;
/// The `dub_voice` value meaning "the speaker's own voice" (no `speech_config`).
pub const DUB_VOICE_SPEAKER: &str = "speaker";
/// The default dub voice — the speaker's own voice (#184 round H step 2).
pub const DEFAULT_DUB_VOICE: &str = DUB_VOICE_SPEAKER;
/// The default dub model — the Live Translate model the round-H probe verified.
pub const DEFAULT_DUB_MODEL: &str = "gemini-3.5-live-translate-preview";

/// The Dabing row's voice line for a stored `dub_voice`: the speaker's own voice
/// reads `hlas: rečník`, a pinned prebuilt voice `hlas: <name>`.
pub fn dub_voice_label(voice: &str) -> String {
    if voice == DUB_VOICE_SPEAKER {
        "hlas: rečník".to_string()
    } else {
        format!("hlas: {voice}")
    }
}

// AI settings (CLIProxyAPI → Claude Opus)
pub const SETTING_AI_API_URL: &str = "ai_api_url";
pub const SETTING_AI_MODEL: &str = "ai_model";
pub const DEFAULT_AI_API_URL: &str = "http://localhost:18787/v1";
/// Claude model used for translation / text cleanup through CLIProxyAPI.
/// `claude-fable-5-1` is the newest Claude flagship the CLIProxyAPI 7.3.1 build
/// on win-resolume routes (#145, proxy upgraded 2026-09-13) — verified live:
/// it is listed by `/v1/models` and a `/v1/chat/completions` call returns
/// HTTP 200 on the existing OAuth login. Supersedes the `claude-opus-4-6`
/// #144 stop-gap, which was itself only needed because the retired 6.9.27
/// proxy build's model registry predated the Claude-5 ids (a request for an
/// unknown id returned `502 unknown provider`; the still-older
/// `claude-opus-4-20250514` also 404'd upstream and cooled down the OAuth
/// auth). Probe a candidate with
/// `python C:\ProgramData\SongPlayer\proxy_probe.py <model>` before changing
/// this — only ids the proxy's `/v1/models` lists will route.
pub const DEFAULT_AI_MODEL: &str = "claude-fable-5-1";

#[cfg(test)]
mod tests {
    use super::*;

    /// The default translation / cleanup model must be the current Claude
    /// flagship that the upgraded CLIProxyAPI actually routes (#145) — never
    /// the `claude-opus-4-6` stop-gap the #144 workaround pinned while the
    /// installed proxy build didn't know the newer ids, and never the long-
    /// retired `claude-opus-4-20250514`. `claude-fable-5-1` was verified live on
    /// win-resolume against CLIProxyAPI 7.3.1 (`/v1/models` lists it and a
    /// `/v1/chat/completions` call returns HTTP 200 on the existing OAuth
    /// login).
    #[test]
    fn default_ai_model_is_current_flagship() {
        assert_eq!(DEFAULT_AI_MODEL, "claude-fable-5-1");
        assert_ne!(
            DEFAULT_AI_MODEL, "claude-opus-4-6",
            "must not remain pinned to the #144 opus-4-6 stop-gap"
        );
        assert_ne!(
            DEFAULT_AI_MODEL, "claude-opus-4-20250514",
            "must not use the retired opus-4 snapshot"
        );
    }

    #[test]
    fn dub_voice_setting_key_and_default_is_the_speaker() {
        assert_eq!(SETTING_DUB_VOICE, "dub_voice");
        assert_eq!(DUB_VOICE_SPEAKER, "speaker");
        assert_eq!(DEFAULT_DUB_VOICE, "speaker");
    }

    #[test]
    fn dub_model_setting_key_and_default() {
        assert_eq!(SETTING_DUB_MODEL, "dub_model");
        assert_eq!(DEFAULT_DUB_MODEL, "gemini-3.5-live-translate-preview");
    }

    #[test]
    fn dub_voice_label_names_the_speaker_in_slovak() {
        assert_eq!(dub_voice_label("speaker"), "hlas: rečník");
        assert_eq!(dub_voice_label("Charon"), "hlas: Charon");
        assert_eq!(dub_voice_label("Kore"), "hlas: Kore");
    }

    #[test]
    fn mix_fader_setting_keys() {
        assert_eq!(SETTING_MIX_SONG_VOKALY, "mix_song_vokaly");
        assert_eq!(SETTING_MIX_SONG_PODKLAD, "mix_song_podklad");
        assert_eq!(SETTING_MIX_DUB_VOKALY, "mix_dub_vokaly");
        assert_eq!(SETTING_MIX_DUB_PODKLAD, "mix_dub_podklad");
        assert_eq!(SETTING_MIX_DUB_DABING, "mix_dub_dabing");
    }
}
