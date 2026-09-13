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

// Default values for settings that have sensible defaults.
pub const DEFAULT_OBS_WEBSOCKET_URL: &str = "ws://127.0.0.1:4455";
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-3.1-pro-preview";
pub const DEFAULT_CACHE_DIR: &str = "cache";
pub const DEFAULT_MAX_RESOLUTION: u32 = 1440;

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
}
