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
/// #210 (B2 of EPIC #174): the program's VBAN audio output (to FOH VB-Matrix
/// and lv1). `"true"` sends; anything else (or absent) = off, the default.
pub const SETTING_VBAN_ENABLED: &str = "vban_enabled";
/// #210: the ASCII VBAN stream name, at most 16 chars
/// ([`DEFAULT_VBAN_STREAM_NAME`] until the B4 switch-over, never cg OBS's `cg`).
pub const SETTING_VBAN_STREAM_NAME: &str = "vban_stream_name";
/// #210: comma-separated `host:port` VBAN targets (default empty = send
/// nothing), e.g. `fohabl.lan:6980, lv1.lan:6980`.
pub const SETTING_VBAN_TARGETS: &str = "vban_targets";
/// #212 (B3 of EPIC #174): the NDI input "OBS manuál" — one received NDI
/// source offered to the program bus. `"true"` receives; anything else (or
/// absent) = off, the default.
pub const SETTING_NDI_INPUT_ENABLED: &str = "ndi_input_enabled";
/// #212: the full NDI name of the received source, `"MACHINE (stream)"` (e.g.
/// cg OBS's manual-scene NDI output). Empty = nothing to receive.
pub const SETTING_NDI_INPUT_SOURCE: &str = "ndi_input_source";

/// #213 (C of EPIC #174): the Companion-compatible remote control, an
/// obs-websocket 5 subset SongPlayer serves on its own port. `"true"` listens;
/// anything else (or absent) = off, the default.
pub const SETTING_REMOTE_WS_ENABLED: &str = "remote_ws_enabled";
/// #213: the remote control's TCP port ([`DEFAULT_REMOTE_WS_PORT`] when absent
/// or not a port).
pub const SETTING_REMOTE_WS_PORT: &str = "remote_ws_port";
/// #213: the optional remote-control password. Set = the obs-websocket 5
/// SHA-256 challenge auth; empty = no auth.
pub const SETTING_REMOTE_WS_PASSWORD: &str = "remote_ws_password";
/// #213: the default remote-control port, next to cg OBS's own 4455.
pub const DEFAULT_REMOTE_WS_PORT: u16 = 4456;

/// #215 (B5 of EPIC #174): `SP-program` follows cg OBS's program scene
/// natively (the scene → source rule of the #213 remote control). `"true"`
/// follows; anything else (or absent) = off, the default.
pub const SETTING_PROGRAM_FOLLOW_OBS: &str = "program_follow_obs";
/// #215: the transition every program cut uses: `obs` (cg OBS's current scene
/// transition, the default), `fade` or `cut`.
pub const SETTING_PROGRAM_TRANSITION: &str = "program_transition";
/// #215: the fade length in ms when SongPlayer picks it (`fade`, or `obs`
/// while cg OBS's transition is not known).
pub const SETTING_PROGRAM_TRANSITION_MS: &str = "program_transition_ms";
/// #215: the default fade length (9 slots of the 30 fps grid).
pub const DEFAULT_PROGRAM_TRANSITION_MS: u32 = 300;
/// #215: the longest fade `SP-program` makes (300 slots of the 30 fps grid); a
/// longer duration is clamped to it.
pub const MAX_PROGRAM_TRANSITION_MS: u32 = 10_000;

/// #215: the fade length a stored `program_transition_ms` means: a positive
/// whole number of ms (trimmed), else [`DEFAULT_PROGRAM_TRANSITION_MS`]. The
/// ONE rule the server and the Nastavenia form share.
pub fn program_transition_ms(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&ms| ms != 0)
        .unwrap_or(DEFAULT_PROGRAM_TRANSITION_MS)
}

/// #212: the program-bus source id of the NDI input (playlists are positive
/// row ids, so a negative id can never collide with one).
pub const PROGRAM_INPUT_ID: i64 = -1;
/// #212: the NDI input's label on the dashboard Program control.
pub const PROGRAM_INPUT_LABEL: &str = "OBS manuál";

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
/// The default VBAN stream name (#210): its own name, so it never collides
/// with cg OBS's `cg` stream before the B4 switch-over.
pub const DEFAULT_VBAN_STREAM_NAME: &str = "sp-program";

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
    /// flagship that the installed CLIProxyAPI actually routes: Opus 5.5 (the
    /// owner, 29.9.2026: "mal by sa pouzivat opus 5.5/sonnet 5.5"). Opus 5.5
    /// needs the proxy to present Claude Code 2.1.280 or newer; CLIProxyAPI
    /// 7.3.1 presented 2.1.258 and got HTTP 400, 8.0.4 (installed 29.9.2026,
    /// #145) returns HTTP 200 on the existing OAuth login. Never one of the
    /// earlier stop-gaps: `claude-fable-5-1` (#145, 13.9.), `claude-opus-4-6`
    /// (#144) or the retired `claude-opus-4-20250514`.
    #[test]
    fn default_ai_model_is_current_flagship() {
        assert_eq!(DEFAULT_AI_MODEL, "claude-opus-5-5");
        for superseded in [
            "claude-fable-5-1",
            "claude-opus-4-6",
            "claude-opus-4-20250514",
        ] {
            assert_ne!(DEFAULT_AI_MODEL, superseded, "superseded model id");
        }
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
    fn vban_setting_keys_and_default_stream_name() {
        assert_eq!(SETTING_VBAN_ENABLED, "vban_enabled");
        assert_eq!(SETTING_VBAN_STREAM_NAME, "vban_stream_name");
        assert_eq!(SETTING_VBAN_TARGETS, "vban_targets");
        assert_eq!(DEFAULT_VBAN_STREAM_NAME, "sp-program");
    }

    #[test]
    fn ndi_input_setting_keys_and_program_id() {
        assert_eq!(SETTING_NDI_INPUT_ENABLED, "ndi_input_enabled");
        assert_eq!(SETTING_NDI_INPUT_SOURCE, "ndi_input_source");
        assert_eq!(PROGRAM_INPUT_ID, -1);
        assert_eq!(PROGRAM_INPUT_LABEL, "OBS manuál");
    }

    #[test]
    fn remote_ws_setting_keys_and_default_port() {
        assert_eq!(SETTING_REMOTE_WS_ENABLED, "remote_ws_enabled");
        assert_eq!(SETTING_REMOTE_WS_PORT, "remote_ws_port");
        assert_eq!(SETTING_REMOTE_WS_PASSWORD, "remote_ws_password");
        assert_eq!(DEFAULT_REMOTE_WS_PORT, 4456);
    }

    #[test]
    fn program_transition_setting_keys_and_default_ms() {
        assert_eq!(SETTING_PROGRAM_FOLLOW_OBS, "program_follow_obs");
        assert_eq!(SETTING_PROGRAM_TRANSITION, "program_transition");
        assert_eq!(SETTING_PROGRAM_TRANSITION_MS, "program_transition_ms");
        assert_eq!(DEFAULT_PROGRAM_TRANSITION_MS, 300);
        assert_eq!(MAX_PROGRAM_TRANSITION_MS, 10_000);
    }

    #[test]
    fn a_stored_fade_length_is_a_positive_whole_number_of_ms_else_300() {
        assert_eq!(program_transition_ms(Some(" 500 ")), 500);
        assert_eq!(program_transition_ms(Some("1")), 1);
        assert_eq!(
            program_transition_ms(Some("20000")),
            20_000,
            "clamped by the slots, not here"
        );
        assert_eq!(
            program_transition_ms(Some("0")),
            300,
            "0 ms is no fade length"
        );
        assert_eq!(program_transition_ms(Some("-5")), 300);
        assert_eq!(program_transition_ms(Some("12.5")), 300);
        assert_eq!(program_transition_ms(Some("abc")), 300);
        assert_eq!(program_transition_ms(Some("")), 300);
        assert_eq!(program_transition_ms(None), 300);
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
