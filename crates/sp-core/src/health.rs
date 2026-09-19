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

use std::time::Duration;

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
        match self {
            HealthTone::Ok => "health-ok",
            HealthTone::Warn => "health-warn",
            HealthTone::Off => "health-off",
        }
    }
}

/// OBS segment: connection + active scene.
/// `OBS: pripojené — <scene>` / `OBS: pripojené` / `OBS: odpojené`.
pub fn obs_label(connected: bool, scene: Option<&str>) -> (HealthTone, String) {
    if !connected {
        return (HealthTone::Warn, "OBS: odpojené".to_string());
    }
    match scene {
        Some(s) if !s.is_empty() => (HealthTone::Ok, format!("OBS: pripojené — {s}")),
        _ => (HealthTone::Ok, "OBS: pripojené".to_string()),
    }
}

/// Resolume segment from the push-chain health snapshot: how many hosts are
/// configured and how many have a problem.
/// `Resolume: —` (no hosts) / `Resolume: OK` / `Resolume: neodpovedá`.
pub fn resolume_label(host_count: usize, problem_count: usize) -> (HealthTone, String) {
    if host_count == 0 {
        (HealthTone::Off, "Resolume: —".to_string())
    } else if problem_count == 0 {
        (HealthTone::Ok, "Resolume: OK".to_string())
    } else {
        (HealthTone::Warn, "Resolume: neodpovedá".to_string())
    }
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
    if !known {
        return (HealthTone::Off, "Nástroje: —".to_string());
    }
    if ytdlp && ffmpeg && js_runtime {
        return (HealthTone::Ok, "Nástroje: OK".to_string());
    }
    let mut missing: Vec<&str> = Vec::new();
    if !ytdlp {
        missing.push("yt-dlp");
    }
    if !ffmpeg {
        missing.push("ffmpeg");
    }
    if !js_runtime {
        missing.push("JS runtime");
    }
    (
        HealthTone::Warn,
        format!("Nástroje: chýba {}", missing.join(", ")),
    )
}

/// WebSocket segment. `WS` label, green when connected else amber.
pub fn ws_label(connected: bool) -> (HealthTone, &'static str) {
    let tone = if connected {
        HealthTone::Ok
    } else {
        HealthTone::Warn
    };
    (tone, "WS")
}

// ---------------------------------------------------------------------------
// #196: post-restart NDI receiver self-check
// ---------------------------------------------------------------------------

/// #196 item 4: the `degraded_reason` an output gets when — 30 s after the
/// startup senders are ready — it is on program (or had a receiver before the
/// restart) yet still has no NDI receiver. Distinct from the dark-wall reason
/// so the receiver-side recovery ladder is NOT run for this class (the ladder
/// cannot clear a restart wedge; only another restart re-rolls it). Shared in
/// `sp_core` so the server sets it and the `HealthBar` counts it by the SAME
/// string.
pub const NO_RECEIVER_AFTER_RESTART_REASON: &str = "no receiver after restart";

/// #196 item 4: how long after the startup senders are ready the post-restart
/// receiver self-check begins. Before this, an output that has not yet been
/// (re)connected is not flagged (DistroAV needs a moment to re-discover the
/// re-advertised senders after a SongPlayer restart).
pub const SELF_CHECK_DELAY: Duration = Duration::from_secs(30);

/// #196 item 4: decide whether an output should be flagged `NO_RECEIVER_AFTER_
/// RESTART_REASON`. Pure so the exact boundaries (on-program vs not, previous
/// count 0 vs 1, exactly 30 s, already-reconnected latch) are unit-tested on
/// Linux with no NDI runtime.
///
/// Returns `true` iff ALL hold:
/// - `elapsed_since_ready` is `Some` and `>= SELF_CHECK_DELAY` (the senders
///   have been ready long enough that a receiver should have re-attached);
/// - the output has NOT reconnected since the restart (once it reaches
///   `connections >= 1` even once, the restart-reconnect succeeded and it is
///   never flagged again this process — a later legitimate off-program drop is
///   not a restart failure);
/// - it currently has no receiver (`connections < 1`);
/// - it is expected to have one: on program now, OR it had `>= 1` receiver
///   recorded before the restart.
pub fn no_receiver_after_restart(
    elapsed_since_ready: Option<Duration>,
    reconnected: bool,
    on_program: bool,
    pre_restart_count: i32,
    connections: i32,
) -> bool {
    let Some(elapsed) = elapsed_since_ready else {
        return false;
    };
    if elapsed < SELF_CHECK_DELAY {
        return false;
    }
    if reconnected {
        return false;
    }
    if connections >= 1 {
        return false;
    }
    on_program || pre_restart_count >= 1
}

/// #196 item 4: Slovak plural noun for the NDI-badge count of outputs without a
/// receiver — 1 `výstup`, 2–4 `výstupy`, else `výstupov`.
pub fn ndi_output_word(n: usize) -> &'static str {
    match n {
        1 => "výstup",
        2..=4 => "výstupy",
        _ => "výstupov",
    }
}

/// #196 item 4: the HealthBar NDI segment (`data-testid="health-ndi"`). `None`
/// when every output has a receiver (the segment is hidden and clears); else a
/// warning naming the count of outputs still dark after the restart.
pub fn ndi_label(outputs_without_receiver: usize) -> Option<(HealthTone, String)> {
    if outputs_without_receiver == 0 {
        return None;
    }
    Some((
        HealthTone::Warn,
        format!(
            "NDI: {n} {w} bez prijímača",
            n = outputs_without_receiver,
            w = ndi_output_word(outputs_without_receiver),
        ),
    ))
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

    // ---- no_receiver_after_restart (#196 item 4) ----

    const D: fn(u64) -> Option<Duration> = |s| Some(Duration::from_secs(s));

    #[test]
    fn self_check_none_elapsed_is_never_flagged() {
        // Senders not marked ready yet → no self-check, whatever the state.
        assert!(!no_receiver_after_restart(None, false, true, 3, 0));
    }

    #[test]
    fn self_check_before_delay_is_not_flagged_boundary() {
        // 29 s < 30 s delay → not yet checked, even on program with 0 receivers.
        assert!(!no_receiver_after_restart(D(29), false, true, 0, 0));
    }

    #[test]
    fn self_check_exactly_at_delay_flags_on_program_dark() {
        // Exactly 30 s (== SELF_CHECK_DELAY): the check is now active.
        assert!(no_receiver_after_restart(D(30), false, true, 0, 0));
    }

    #[test]
    fn self_check_reconnected_output_is_never_flagged() {
        // Reached >=1 once since restart → cleared for good this process, even
        // if it is momentarily 0 again.
        assert!(!no_receiver_after_restart(D(45), true, true, 3, 0));
    }

    #[test]
    fn self_check_with_a_receiver_is_not_flagged_boundary() {
        // connections == 1 (has a receiver) → not dark.
        assert!(!no_receiver_after_restart(D(45), false, true, 3, 1));
    }

    #[test]
    fn self_check_on_program_dark_is_flagged() {
        assert!(no_receiver_after_restart(D(45), false, true, 0, 0));
    }

    #[test]
    fn self_check_off_program_previously_connected_is_flagged() {
        // Not on program now, but had a receiver before the restart (pre==1).
        assert!(no_receiver_after_restart(D(45), false, false, 1, 0));
    }

    #[test]
    fn self_check_off_program_never_connected_is_not_flagged() {
        // Off program and never had a receiver (pre==0) → legitimately 0, not a
        // restart failure.
        assert!(!no_receiver_after_restart(D(45), false, false, 0, 0));
    }

    #[test]
    fn self_check_off_program_pre_count_boundary() {
        // pre_restart_count boundary: exactly 1 flags, 0 does not.
        assert!(no_receiver_after_restart(D(45), false, false, 1, 0));
        assert!(!no_receiver_after_restart(D(45), false, false, 0, 0));
    }

    #[test]
    fn self_check_negative_connections_counts_as_dark() {
        // -1 ("never polled") is < 1 → dark.
        assert!(no_receiver_after_restart(D(45), false, true, 0, -1));
    }

    // ---- ndi_output_word + ndi_label (#196 item 4) ----

    #[test]
    fn ndi_word_singular() {
        assert_eq!(ndi_output_word(1), "výstup");
    }

    #[test]
    fn ndi_word_paucal_2_to_4() {
        assert_eq!(ndi_output_word(2), "výstupy");
        assert_eq!(ndi_output_word(4), "výstupy");
    }

    #[test]
    fn ndi_word_plural_5_and_zero() {
        assert_eq!(ndi_output_word(5), "výstupov");
        assert_eq!(ndi_output_word(0), "výstupov");
    }

    #[test]
    fn ndi_label_zero_is_none() {
        assert_eq!(ndi_label(0), None);
    }

    #[test]
    fn ndi_label_one_is_warn_singular() {
        let (tone, text) = ndi_label(1).expect("1 output flagged");
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "NDI: 1 výstup bez prijímača");
    }

    #[test]
    fn ndi_label_three_is_warn_paucal() {
        assert_eq!(ndi_label(3).unwrap().1, "NDI: 3 výstupy bez prijímača");
    }

    #[test]
    fn ndi_label_five_is_warn_plural() {
        assert_eq!(ndi_label(5).unwrap().1, "NDI: 5 výstupov bez prijímača");
    }
}
