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

/// #215: the transition every program cut uses: `fade` or `cut`; anything
/// else (or absent) is the default fade of [`SETTING_PROGRAM_TRANSITION_MS`].
/// #221 L5 retired `obs` (cg OBS's scene transition) with the OBS follow.
pub const SETTING_PROGRAM_TRANSITION: &str = "program_transition";
/// #215: the fade length in ms (`fade`, and the default).
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

/// #223 S2: the `SP-program-MAX` output — the program's fixed 3840×2160
/// picture, composed on the GPU and shared with Resolume Arena over Spout.
/// ON unless the setting says exactly `"false"` ([`program_max_enabled`]).
pub const SETTING_PROGRAM_MAX_ENABLED: &str = "program_max_enabled";
/// #223 S2: MAX is ON by default (the owner decided MAX exists, revision 3).
pub const DEFAULT_PROGRAM_MAX_ENABLED: bool = true;

/// #223 S2: whether a stored `program_max_enabled` turns `SP-program-MAX` on:
/// OFF only for an explicit `"false"` (trimmed), else
/// [`DEFAULT_PROGRAM_MAX_ENABLED`]. The ONE rule the startup read and the
/// settings task share.
pub fn program_max_enabled(raw: Option<&str>) -> bool {
    raw.map_or(DEFAULT_PROGRAM_MAX_ENABLED, |v| v.trim() != "false")
}

/// #223 S3b: hardware video decode for playback — Media Foundation's decoder
/// on the GPU (a Direct3D 11 device manager) instead of in software. The
/// paced decode producer reads it when it opens a song, so a change applies
/// from the next song ([`video_hw_decode`]).
pub const SETTING_VIDEO_HW_DECODE: &str = "video_hw_decode";
/// #223 S3b: OFF until the main session's box gate passes (4K decode mean
/// ≤ 50 % of 1/f, 1440p not slower); the decode bench measures it with
/// `"hw": true` meanwhile.
pub const DEFAULT_VIDEO_HW_DECODE: bool = false;

/// #223 S3b: whether a stored `video_hw_decode` turns hardware decode on: ON
/// only for an explicit `"true"` (trimmed), else
/// [`DEFAULT_VIDEO_HW_DECODE`]. The ONE rule the startup read and the
/// settings task share.
pub fn video_hw_decode(raw: Option<&str>) -> bool {
    raw.map_or(DEFAULT_VIDEO_HW_DECODE, |v| v.trim() == "true")
}

/// #233: the program's audio outputs, one JSON list (`crate::audio_outputs`).
pub const SETTING_AUDIO_OUTPUTS: &str = "audio_outputs";
/// #233: the audio network's sample rate, Hz; an output whose rate is
/// "network" runs at it.
pub const SETTING_AUDIO_NETWORK_RATE: &str = "audio_network_rate";

/// #233: the stored network rate: a supported rate, else 48 kHz.
pub fn audio_network_rate(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|r| crate::audio_outputs::SUPPORTED_RATES.contains(r))
        .unwrap_or(crate::audio_outputs::DEFAULT_NETWORK_RATE)
}

// #229: the node exchange — SongPlayer sites (SNV, PP) share processed content.
/// This node's name in the exchange (`snv`, `pp`); empty = the exchange is off.
pub const SETTING_NODE_NAME: &str = "node_name";
/// The key this node's peer API will accept (`X-SP-Peer-Key`, lane 4); empty = not serving.
pub const SETTING_PEER_API_KEY: &str = "peer_api_key";
/// The peers this node asks before a heavy job: a JSON list (sp-server `peer::config`).
pub const SETTING_PEERS: &str = "peers";
/// "true" stops new peer transfers both ways (an operator's pause, later #230's).
pub const SETTING_PEER_TRANSFERS_PAUSED: &str = "peer_transfers_paused";
/// The most this node SENDS to its peers, in Mbit/s (its uplink also carries the live stream).
pub const SETTING_PEER_SERVE_MAX_MBPS: &str = "peer_serve_max_mbps";
pub const DEFAULT_PEER_SERVE_MAX_MBPS: u32 = 20;
pub const MAX_PEER_SERVE_MAX_MBPS: u32 = 10_000;

/// #229: transfers pause only when the setting says exactly "true".
pub fn peer_transfers_paused(raw: Option<&str>) -> bool {
    raw.map(str::trim) == Some("true")
}

/// #229: the upload cap in Mbit/s: a whole number in 1..=10000, else the default.
pub fn peer_serve_max_mbps(raw: Option<&str>) -> u32 {
    raw.and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| (1..=MAX_PEER_SERVE_MAX_MBPS).contains(v))
        .unwrap_or(DEFAULT_PEER_SERVE_MAX_MBPS)
}

/// #229: the Genius lyrics token (read by the lyrics worker from the DB).
pub const SETTING_GENIUS_ACCESS_TOKEN: &str = "genius_access_token";

/// #229 item C (the owner's ruling, 8.10.2026): whether this node may call
/// paid AI (Gemini, Claude) — the ONE switch sp-server's `paid_ai` gates
/// every such call on. Not a secret.
pub const SETTING_PAID_AI_ENABLED: &str = "paid_ai_enabled";

/// #229 item C: the stored `paid_ai_enabled` read. Unset or blank = ON (a
/// node that never set it, e.g. SNV, is unchanged); `"true"` = ON (trimmed,
/// any case); `"false"` — or any other value, which only a write past the
/// settings API can store — = OFF: the owner's money comes first.
pub fn paid_ai_enabled(raw: Option<&str>) -> bool {
    match raw.map(|v| v.trim().to_ascii_lowercase()) {
        None => true,
        Some(v) => v.is_empty() || v == "true",
    }
}

/// #229 item C: what a Nastavenia save sends for the switch — `"true"` /
/// `"false"` only when the checkbox differs from the value the page loaded
/// (`loaded`, read by [`paid_ai_enabled`]), else nothing: a tab opened
/// before the switch was changed elsewhere never sends the old one back.
pub fn paid_ai_to_send(loaded: Option<&str>, checked: bool) -> Option<String> {
    (paid_ai_enabled(loaded) != checked).then(|| checked.to_string())
}

/// #229: what a secret setting reads as outside the node (`GET /api/v1/settings`).
/// A PATCH that sends it back keeps the stored value.
pub const SECRET_MASK: &str = "********";

/// #229: the secret settings this app reads or writes — keys, tokens,
/// passwords. THE list: `GET /api/v1/settings` masks them (sp-server
/// `api::settings`), the dashboard shows the mask. Workers read the stored
/// value from the DB, never through the API. (`peers` is not on it: it holds
/// its secrets INSIDE a JSON list; sp-server masks those fields.)
pub const SECRET_SETTINGS: &[&str] = &[
    SETTING_GEMINI_API_KEY,
    SETTING_GENIUS_ACCESS_TOKEN,
    SETTING_OBS_WEBSOCKET_PASSWORD,
    SETTING_PEER_API_KEY,
    SETTING_REMOTE_WS_PASSWORD,
];

/// #229: a setting NAMED like a credential is secret too, listed or not: the
/// settings table keeps every row ever written, e.g. a retired provider's
/// `replicate_api_token` / `assemblyai_api_key` (#159 deleted their code, not
/// the rows), or a key an operator PATCHed by hand.
pub const SECRET_SETTING_SUFFIXES: &[&str] = &["_key", "_token", "_password", "_secret"];

/// #229: whether `key` is a secret setting: on [`SECRET_SETTINGS`], or named
/// with one of [`SECRET_SETTING_SUFFIXES`].
pub fn is_secret_setting(key: &str) -> bool {
    SECRET_SETTINGS.contains(&key) || SECRET_SETTING_SUFFIXES.iter().any(|s| key.ends_with(s))
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

// AI settings (CLIProxyAPI → Claude Opus 5.5)
pub const SETTING_AI_API_URL: &str = "ai_api_url";
pub const SETTING_AI_MODEL: &str = "ai_model";
pub const DEFAULT_AI_API_URL: &str = "http://localhost:18787/v1";
/// Claude model used for translation / text cleanup through CLIProxyAPI.
/// `claude-opus-5-5` is the newest Claude flagship (owner, 29.9.2026). It
/// needs the proxy to present Claude Code 2.1.280 or newer: CLIProxyAPI 7.3.1
/// presented 2.1.258 and got HTTP 400, 8.0.4 (installed 29.9.2026, #145)
/// answers HTTP 200 on the existing OAuth login. The post-deploy AI step makes
/// a real completion with the model SongPlayer sends and fails when the proxy
/// lists a newer `claude-opus-*`, so a model the proxy cannot serve, or a newer
/// Opus nobody switched to, fails CI. Probe a candidate with
/// `python C:\ProgramData\SongPlayer\proxy_probe.py <model>` before changing
/// this — only ids the proxy's `/v1/models` lists will route.
pub const DEFAULT_AI_MODEL: &str = "claude-opus-5-5";

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
    fn the_default_vban_stream_name() {
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

    /// #223 S2: `SP-program-MAX` is ON unless the setting says exactly
    /// "false" — the owner decided MAX exists, so a missing or mangled value
    /// keeps it on.
    #[test]
    fn program_max_is_on_unless_the_setting_says_false() {
        assert_eq!(SETTING_PROGRAM_MAX_ENABLED, "program_max_enabled");
        assert!(program_max_enabled(None), "no setting = ON");
        assert!(program_max_enabled(Some("true")));
        assert!(program_max_enabled(Some("")), "an empty value = ON");
        assert!(program_max_enabled(Some("no?")), "a mangled value = ON");
        assert!(
            !program_max_enabled(Some("false")),
            "only an explicit false"
        );
        assert!(!program_max_enabled(Some(" false\n")), "trimmed");
    }

    /// #223 S3b: hardware decode is OFF unless the setting says exactly
    /// "true" — playback stays on the measured software path until the box
    /// gate passes, so a missing or mangled value keeps it off.
    #[test]
    fn video_hw_decode_is_off_unless_the_setting_says_true() {
        assert_eq!(SETTING_VIDEO_HW_DECODE, "video_hw_decode");
        assert!(!video_hw_decode(None), "no setting = OFF");
        assert!(video_hw_decode(Some("true")));
        assert!(video_hw_decode(Some(" true\n")), "trimmed");
        assert!(!video_hw_decode(Some("false")));
        assert!(!video_hw_decode(Some("")), "an empty value = OFF");
        assert!(!video_hw_decode(Some("TRUE")), "only the exact word");
        assert!(!video_hw_decode(Some("yes")), "a mangled value = OFF");
    }

    #[test]
    fn mix_fader_setting_keys() {
        assert_eq!(SETTING_MIX_SONG_VOKALY, "mix_song_vokaly");
        assert_eq!(SETTING_MIX_SONG_PODKLAD, "mix_song_podklad");
        assert_eq!(SETTING_MIX_DUB_VOKALY, "mix_dub_vokaly");
        assert_eq!(SETTING_MIX_DUB_PODKLAD, "mix_dub_podklad");
        assert_eq!(SETTING_MIX_DUB_DABING, "mix_dub_dabing");
    }

    #[test]
    fn exchange_setting_keys() {
        assert_eq!(SETTING_NODE_NAME, "node_name");
        assert_eq!(SETTING_PEER_API_KEY, "peer_api_key");
        assert_eq!(SETTING_PEERS, "peers");
        assert_eq!(SETTING_PEER_TRANSFERS_PAUSED, "peer_transfers_paused");
        assert_eq!(SETTING_PEER_SERVE_MAX_MBPS, "peer_serve_max_mbps");
    }

    /// #229: peer transfers pause only on an explicit "true"; a missing or
    /// mangled value keeps them running.
    #[test]
    fn peer_transfers_pause_only_on_true() {
        assert!(!peer_transfers_paused(None));
        assert!(peer_transfers_paused(Some("true")));
        assert!(peer_transfers_paused(Some(" true\n")), "trimmed");
        assert!(!peer_transfers_paused(Some("TRUE")), "only the exact word");
        assert!(!peer_transfers_paused(Some("1")));
        assert!(!peer_transfers_paused(Some("")));
    }

    /// #229: what this node sends to peers is capped at 1..=10000 Mbit/s;
    /// anything else reads as the 20 Mbit/s default.
    #[test]
    fn peer_serve_cap_is_1_to_10000_mbps_else_20() {
        assert_eq!(peer_serve_max_mbps(None), 20);
        assert_eq!(peer_serve_max_mbps(Some("50")), 50);
        assert_eq!(peer_serve_max_mbps(Some(" 7 ")), 7);
        assert_eq!(peer_serve_max_mbps(Some("1")), 1);
        assert_eq!(peer_serve_max_mbps(Some("10000")), 10_000);
        assert_eq!(peer_serve_max_mbps(Some("10001")), 20);
        assert_eq!(peer_serve_max_mbps(Some("0")), 20);
        assert_eq!(peer_serve_max_mbps(Some("-1")), 20);
        assert_eq!(peer_serve_max_mbps(Some("fast")), 20);
    }

    /// #229 item C: paid AI is ON unless the switch says off; a value the
    /// API would refuse reads as off.
    #[test]
    fn paid_ai_is_on_unless_the_switch_says_off() {
        assert_eq!(SETTING_PAID_AI_ENABLED, "paid_ai_enabled");
        assert!(!is_secret_setting(SETTING_PAID_AI_ENABLED));
        assert!(paid_ai_enabled(None), "unset = ON (SNV unchanged)");
        assert!(paid_ai_enabled(Some("")), "blank = unset");
        assert!(paid_ai_enabled(Some("  ")), "blank = unset");
        assert!(paid_ai_enabled(Some("true")));
        assert!(paid_ai_enabled(Some(" TRUE\n")), "trimmed, any case");
        assert!(!paid_ai_enabled(Some("false")));
        assert!(!paid_ai_enabled(Some(" False ")));
        assert!(!paid_ai_enabled(Some("yes")), "a mangled value = OFF");
        assert!(!paid_ai_enabled(Some("1")));
    }

    /// #229 item C: a save sends the switch only when the checkbox changed
    /// it from what the page loaded.
    #[test]
    fn a_save_sends_the_switch_only_when_it_changed() {
        assert_eq!(paid_ai_to_send(None, true), None, "on as loaded");
        assert_eq!(paid_ai_to_send(None, false).as_deref(), Some("false"));
        assert_eq!(
            paid_ai_to_send(Some("false"), true).as_deref(),
            Some("true")
        );
        assert_eq!(paid_ai_to_send(Some("false"), false), None, "off as loaded");
        assert_eq!(paid_ai_to_send(Some("true"), true), None);
    }

    /// #229: THE secret list, exactly; each one masked by name too.
    #[test]
    fn the_secret_settings_list() {
        assert_eq!(SECRET_MASK, "********");
        assert_eq!(SETTING_GENIUS_ACCESS_TOKEN, "genius_access_token");
        assert_eq!(
            SECRET_SETTINGS,
            &[
                "gemini_api_key",
                "genius_access_token",
                "obs_websocket_password",
                "peer_api_key",
                "remote_ws_password",
            ]
        );
        for key in SECRET_SETTINGS {
            assert!(is_secret_setting(key), "{key}");
        }
    }

    /// #229: a setting named like a credential is secret even unlisted (a
    /// retired provider's row still in the DB); every other setting is not.
    #[test]
    fn a_setting_named_like_a_credential_is_secret() {
        for key in [
            "replicate_api_token",
            "assemblyai_api_key",
            "some_secret",
            "x_password",
        ] {
            assert!(is_secret_setting(key), "{key}");
        }
        for key in [
            "gemini_model",
            "obs_websocket_url",
            "cache_dir",
            "peers",
            "node_name",
            "peer_transfers_paused",
            "peer_serve_max_mbps",
            "remote_ws_port",
            "token",
            "keyboard",
            "api_key_hint",
            "",
        ] {
            assert!(!is_secret_setting(key), "{key}");
        }
    }
}
