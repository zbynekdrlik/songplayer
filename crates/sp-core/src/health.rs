//! #194 ROUND 3b: ONE health-strip vocabulary for the app's `HealthBar`.
//!
//! Before #194 the status badges (OBS / genlock / Resolume / tools / LAN /
//! version) lived only on the Dashboard, each with its own wording. This module
//! is the single source of truth for the OBS / Resolume / tools / WS label +
//! tone, so the SAME Slovak strip renders identically on every page.
//!
//! It lives in `sp_core` (not sp-ui) because sp-ui has no unit-test job — the
//! label/tone mapping is covered here by the workspace `Test` job and the
//! diff-scoped mutation gate, and sp-ui renders whatever these return.
//!
//! Slovak everywhere; the only fixed English is the proper names (OBS,
//! Resolume, WS) — per `.claude/rules/sp-ui-frontend.md`.

/// Colour family for a health segment. sp-ui maps each to one CSS class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthTone {
    /// Healthy / connected — green.
    Ok,
    /// A real problem the operator should see — amber/red.
    Warn,
    /// Not applicable / unknown yet — grey.
    Off,
}

impl HealthTone {
    /// CSS class for this tone (one colour per state, everywhere).
    pub fn css_class(self) -> &'static str {
        todo!()
    }
}

/// OBS segment: connection + active scene.
/// `OBS: pripojené — <scene>` / `OBS: pripojené` / `OBS: odpojené`.
pub fn obs_label(connected: bool, scene: Option<&str>) -> (HealthTone, String) {
    let _ = (connected, scene);
    todo!()
}

/// Resolume segment from the push-chain health snapshot: how many hosts are
/// configured and how many have a problem.
/// `Resolume: —` (no hosts) / `Resolume: OK` / `Resolume: neodpovedá`.
pub fn resolume_label(host_count: usize, problem_count: usize) -> (HealthTone, String) {
    let _ = (host_count, problem_count);
    todo!()
}

/// Tools segment from the `ToolsStatus` payload. `known == false` means no
/// `ToolsStatus` has arrived yet (grey `—`). Otherwise `Nástroje: OK` when all
/// three are present, else `Nástroje: chýba <list>` naming each missing one.
pub fn tools_label(
    known: bool,
    ytdlp: bool,
    ffmpeg: bool,
    js_runtime: bool,
) -> (HealthTone, String) {
    let _ = (known, ytdlp, ffmpeg, js_runtime);
    todo!()
}

/// WebSocket segment. `WS` label, green when connected else amber.
pub fn ws_label(connected: bool) -> (HealthTone, &'static str) {
    let _ = connected;
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- css_class ----

    #[test]
    fn tone_css_classes() {
        assert_eq!(HealthTone::Ok.css_class(), "health-ok");
        assert_eq!(HealthTone::Warn.css_class(), "health-warn");
        assert_eq!(HealthTone::Off.css_class(), "health-off");
    }

    // ---- obs_label ----

    #[test]
    fn obs_connected_with_scene() {
        let (tone, text) = obs_label(true, Some("sp-alex"));
        assert_eq!(tone, HealthTone::Ok);
        assert_eq!(text, "OBS: pripojené — sp-alex");
    }

    #[test]
    fn obs_connected_no_scene() {
        let (tone, text) = obs_label(true, None);
        assert_eq!(tone, HealthTone::Ok);
        assert_eq!(text, "OBS: pripojené");
    }

    #[test]
    fn obs_connected_empty_scene_reads_as_no_scene() {
        let (tone, text) = obs_label(true, Some(""));
        assert_eq!(tone, HealthTone::Ok);
        assert_eq!(text, "OBS: pripojené");
    }

    #[test]
    fn obs_disconnected() {
        let (tone, text) = obs_label(false, Some("sp-alex"));
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "OBS: odpojené");
    }

    // ---- resolume_label ----

    #[test]
    fn resolume_no_hosts() {
        let (tone, text) = resolume_label(0, 0);
        assert_eq!(tone, HealthTone::Off);
        assert_eq!(text, "Resolume: —");
    }

    #[test]
    fn resolume_all_healthy() {
        let (tone, text) = resolume_label(2, 0);
        assert_eq!(tone, HealthTone::Ok);
        assert_eq!(text, "Resolume: OK");
    }

    #[test]
    fn resolume_one_host_healthy_boundary() {
        assert_eq!(resolume_label(1, 0).1, "Resolume: OK");
    }

    #[test]
    fn resolume_one_problem() {
        let (tone, text) = resolume_label(2, 1);
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "Resolume: neodpovedá");
    }

    #[test]
    fn resolume_all_problems() {
        let (tone, text) = resolume_label(3, 3);
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "Resolume: neodpovedá");
    }

    // ---- tools_label ----

    #[test]
    fn tools_unknown() {
        let (tone, text) = tools_label(false, true, true, true);
        assert_eq!(tone, HealthTone::Off);
        assert_eq!(text, "Nástroje: —");
    }

    #[test]
    fn tools_all_ok() {
        let (tone, text) = tools_label(true, true, true, true);
        assert_eq!(tone, HealthTone::Ok);
        assert_eq!(text, "Nástroje: OK");
    }

    #[test]
    fn tools_missing_ytdlp() {
        let (tone, text) = tools_label(true, false, true, true);
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "Nástroje: chýba yt-dlp");
    }

    #[test]
    fn tools_missing_ffmpeg() {
        let (_, text) = tools_label(true, true, false, true);
        assert_eq!(text, "Nástroje: chýba ffmpeg");
    }

    #[test]
    fn tools_missing_js_runtime() {
        let (_, text) = tools_label(true, true, true, false);
        assert_eq!(text, "Nástroje: chýba JS runtime");
    }

    #[test]
    fn tools_missing_multiple_joined_in_order() {
        let (tone, text) = tools_label(true, false, false, false);
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "Nástroje: chýba yt-dlp, ffmpeg, JS runtime");
    }

    #[test]
    fn tools_missing_ytdlp_and_js_runtime() {
        assert_eq!(
            tools_label(true, false, true, false).1,
            "Nástroje: chýba yt-dlp, JS runtime"
        );
    }

    // ---- ws_label ----

    #[test]
    fn ws_connected() {
        let (tone, text) = ws_label(true);
        assert_eq!(tone, HealthTone::Ok);
        assert_eq!(text, "WS");
    }

    #[test]
    fn ws_disconnected() {
        let (tone, text) = ws_label(false);
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "WS");
    }
}
