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

use std::collections::BTreeMap;

/// Colour family for a health segment. sp-ui maps each to one CSS class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthTone {
    /// Healthy / connected — green.
    Ok,
    /// A real problem the operator should see — amber/red.
    Warn,
    /// Not applicable / unknown yet — grey.
    Off,
    /// #233: a box that must not run like this (the production environment
    /// is violated) — red.
    Error,
}

impl HealthTone {
    /// CSS class for this tone (one colour per state, everywhere).
    pub fn css_class(self) -> &'static str {
        match self {
            HealthTone::Ok => "health-ok",
            HealthTone::Warn => "health-warn",
            HealthTone::Off => "health-off",
            HealthTone::Error => "health-error",
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

/// #262: one Resolume host as the health bar reads it. A PROBLEM is only a
/// host that does not answer; a SongPlayer token the composition has no clip
/// for is a NOTE: the operator's setup (PP shows no English line), never an
/// error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolumeHostView {
    /// Why the host does not answer, or `None`.
    pub problem: Option<String>,
    /// The SongPlayer tokens the composition has no clip for, or `None`.
    pub note: Option<String>,
    /// The composition has a SongPlayer clip (or was not read yet).
    pub has_clips: bool,
}

/// #262: a host's view from its `GET /api/v1/resolume/health` row. An empty
/// token map is a host not read yet: no note, and not counted as clipless.
pub fn resolume_host_view(
    circuit_open: bool,
    consecutive_failures: u32,
    clips_by_token: &BTreeMap<String, usize>,
) -> ResolumeHostView {
    let problem = if circuit_open {
        Some("okruh otvorený — Resolume nedostupné".to_string())
    } else if consecutive_failures > 0 {
        Some(format!("obnova zlyháva ({consecutive_failures} po sebe)"))
    } else {
        None
    };
    let missing: Vec<&str> = clips_by_token
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(token, _)| token.as_str())
        .collect();
    let has_clips = clips_by_token.is_empty() || missing.len() < clips_by_token.len();
    let note = if !has_clips {
        Some("v kompozícii nie je žiadny klip SongPlayera".to_string())
    } else if missing.is_empty() {
        None
    } else {
        Some(format!(
            "v kompozícii nie sú klipy {} (nastavenie Resolume, nie chyba)",
            missing.join(", ")
        ))
    };
    ResolumeHostView {
        problem,
        note,
        has_clips,
    }
}

/// Resolume segment from the hosts' views: how many hosts are configured,
/// how many do not answer and how many have no SongPlayer clip.
/// `Resolume: —` (no hosts) / `Resolume: neodpovedá` / `Resolume: bez klipov
/// SongPlayera` (every host answers, none has a clip) / `Resolume: OK`.
pub fn resolume_label(
    host_count: usize,
    unreachable_count: usize,
    clipless_count: usize,
) -> (HealthTone, String) {
    if host_count == 0 {
        (HealthTone::Off, "Resolume: —".to_string())
    } else if unreachable_count > 0 {
        (HealthTone::Warn, "Resolume: neodpovedá".to_string())
    } else if clipless_count == host_count {
        (
            HealthTone::Off,
            "Resolume: bez klipov SongPlayera".to_string(),
        )
    } else {
        (HealthTone::Ok, "Resolume: OK".to_string())
    }
}

/// #262: the segment's tooltip: `host: problem` or `host: note` per host
/// that has one (a problem wins), ` | ` between.
pub fn resolume_title(hosts: &[(String, ResolumeHostView)]) -> String {
    hosts
        .iter()
        .filter_map(|(host, view)| {
            view.problem
                .as_ref()
                .or(view.note.as_ref())
                .map(|text| format!("{host}: {text}"))
        })
        .collect::<Vec<_>>()
        .join(" | ")
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

/// #229 item C: the node segment, `Uzol: <name>`; `None` (no segment) for
/// a node with no name (the exchange off).
pub fn node_label(name: Option<&str>) -> Option<String> {
    name.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| format!("Uzol: {n}"))
}

/// #229 item C: the paid-AI segment, shown only while the node's switch is
/// off (`Some(false)`): `Platené AI: vypnuté`, amber, with the kinds of work
/// it holds as the tooltip. On, or unknown (an older server), shows nothing.
pub fn paid_ai_label(
    enabled: Option<bool>,
    held: &[String],
) -> Option<(HealthTone, String, String)> {
    if enabled != Some(false) {
        return None;
    }
    let tip = if held.is_empty() {
        format!("Tento uzol nevolá platené AI (Gemini, Claude) — {FROM_PEER}")
    } else {
        format!(
            "Čaká, kým sa platené AI zapne: {} ({FROM_PEER})",
            held.iter()
                .map(|k| held_kind_sk(k.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Some((HealthTone::Warn, "Platené AI: vypnuté".to_string(), tip))
}

/// What a node with paid AI off still takes: a peer's lyrics and titles.
const FROM_PEER: &str = "texty a metadáta berie medzitým od susedného uzla";

/// A held kind (`paid_ai_held` on `GET /api/v1/status`) in Slovak.
fn held_kind_sk(kind: &str) -> &str {
    match kind {
        "lyrics" => "texty",
        "translation" => "preklad",
        "metadata" => "metadáta",
        "dub" => "dabing",
        other => other,
    }
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
        assert_eq!(HealthTone::Error.css_class(), "health-error");
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
        let (tone, text) = resolume_label(0, 0, 0);
        assert_eq!(tone, HealthTone::Off);
        assert_eq!(text, "Resolume: —");
    }

    #[test]
    fn resolume_all_healthy() {
        let (tone, text) = resolume_label(2, 0, 0);
        assert_eq!(tone, HealthTone::Ok);
        assert_eq!(text, "Resolume: OK");
    }

    #[test]
    fn resolume_one_host_healthy_boundary() {
        assert_eq!(resolume_label(1, 0, 0).1, "Resolume: OK");
    }

    #[test]
    fn resolume_one_problem() {
        let (tone, text) = resolume_label(2, 1, 0);
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "Resolume: neodpovedá");
    }

    #[test]
    fn resolume_all_problems() {
        let (tone, text) = resolume_label(3, 3, 3);
        assert_eq!(tone, HealthTone::Warn);
        assert_eq!(text, "Resolume: neodpovedá", "not answering wins");
    }

    /// #262: every host answers but none has a SongPlayer clip: grey, never
    /// amber; one host with a clip makes it OK.
    #[test]
    fn resolume_with_no_songplayer_clip_is_grey() {
        let (tone, text) = resolume_label(1, 0, 1);
        assert_eq!(tone, HealthTone::Off);
        assert_eq!(text, "Resolume: bez klipov SongPlayera");
        assert_eq!(
            resolume_label(2, 0, 2).1,
            "Resolume: bez klipov SongPlayera"
        );
        assert_eq!(
            resolume_label(2, 0, 1),
            (HealthTone::Ok, "Resolume: OK".to_string())
        );
    }

    // ---- resolume_host_view (#262) ----

    fn tokens(counts: &[(&str, usize)]) -> BTreeMap<String, usize> {
        counts.iter().map(|(t, n)| (t.to_string(), *n)).collect()
    }

    /// PP's live row (13.10.2026): Arena answers, its composition has no
    /// English-line clips. Not a problem — a note naming the tokens.
    #[test]
    fn pp_s_composition_without_english_lines_answers_with_a_note() {
        let view = resolume_host_view(
            false,
            0,
            &tokens(&[
                ("#sp-subs", 0),
                ("#sp-subs-next", 0),
                ("#sp-subssk", 3),
                ("#sp-title", 4),
            ]),
        );
        assert_eq!(
            view,
            ResolumeHostView {
                problem: None,
                note: Some(
                    "v kompozícii nie sú klipy #sp-subs, #sp-subs-next (nastavenie Resolume, nie chyba)"
                        .to_string()
                ),
                has_clips: true,
            }
        );
    }

    #[test]
    fn a_composition_with_every_token_has_no_note() {
        let view = resolume_host_view(
            false,
            0,
            &tokens(&[
                ("#sp-subs", 4),
                ("#sp-subs-next", 1),
                ("#sp-subssk", 5),
                ("#sp-title", 5),
            ]),
        );
        assert_eq!(
            (view.problem, view.note, view.has_clips),
            (None, None, true)
        );
    }

    #[test]
    fn a_composition_with_no_songplayer_clip_is_clipless_not_a_problem() {
        let view = resolume_host_view(false, 0, &tokens(&[("#sp-subs", 0), ("#sp-title", 0)]));
        assert_eq!(
            view,
            ResolumeHostView {
                problem: None,
                note: Some("v kompozícii nie je žiadny klip SongPlayera".to_string()),
                has_clips: false,
            }
        );
    }

    /// One token with one clip is enough to have clips.
    #[test]
    fn one_clip_of_one_token_counts_as_clips() {
        let view = resolume_host_view(false, 0, &tokens(&[("#sp-subs", 0), ("#sp-title", 1)]));
        assert!(view.has_clips);
        assert_eq!(
            view.note.as_deref(),
            Some("v kompozícii nie sú klipy #sp-subs (nastavenie Resolume, nie chyba)")
        );
    }

    /// A host not read yet (no token map): neither a note nor clipless.
    #[test]
    fn a_host_not_read_yet_has_no_note() {
        let view = resolume_host_view(false, 0, &BTreeMap::new());
        assert_eq!(
            (view.problem, view.note, view.has_clips),
            (None, None, true)
        );
    }

    #[test]
    fn only_a_host_that_does_not_answer_is_a_problem() {
        let some = tokens(&[("#sp-title", 2)]);
        assert_eq!(
            resolume_host_view(true, 0, &some).problem.as_deref(),
            Some("okruh otvorený — Resolume nedostupné")
        );
        assert_eq!(
            resolume_host_view(true, 3, &some).problem.as_deref(),
            Some("okruh otvorený — Resolume nedostupné"),
            "the open breaker is named first"
        );
        assert_eq!(
            resolume_host_view(false, 2, &some).problem.as_deref(),
            Some("obnova zlyháva (2 po sebe)")
        );
        assert_eq!(resolume_host_view(false, 0, &some).problem, None);
    }

    // ---- resolume_title (#262) ----

    #[test]
    fn the_title_names_each_host_s_problem_or_note() {
        let pp = resolume_host_view(
            false,
            0,
            &tokens(&[("#sp-subs", 0), ("#sp-subssk", 3), ("#sp-title", 4)]),
        );
        let down = resolume_host_view(false, 1, &tokens(&[("#sp-subs", 0), ("#sp-title", 2)]));
        let fine = resolume_host_view(false, 0, &tokens(&[("#sp-title", 2)]));
        let hosts = vec![
            ("127.0.0.1".to_string(), pp),
            ("10.0.0.2".to_string(), down),
            ("10.0.0.3".to_string(), fine),
        ];
        assert_eq!(
            resolume_title(&hosts),
            "127.0.0.1: v kompozícii nie sú klipy #sp-subs (nastavenie Resolume, nie chyba) \
             | 10.0.0.2: obnova zlyháva (1 po sebe)"
        );
        assert_eq!(resolume_title(&hosts[2..]), "", "nothing to say");
        assert_eq!(resolume_title(&[]), "");
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

    /// #229 item C: the node's name, when it has one.
    #[test]
    fn node_label_names_the_node() {
        assert_eq!(node_label(Some("pp")).as_deref(), Some("Uzol: pp"));
        assert_eq!(node_label(Some(" snv ")).as_deref(), Some("Uzol: snv"));
        assert_eq!(node_label(Some("  ")), None);
        assert_eq!(node_label(None), None);
    }

    /// #229 item C: the segment shows only while paid AI is off, naming
    /// what it holds in Slovak.
    #[test]
    fn paid_ai_label_shows_only_while_off() {
        assert_eq!(paid_ai_label(Some(true), &[]), None);
        assert_eq!(paid_ai_label(None, &["lyrics".to_string()]), None);
        let (tone, text, tip) = paid_ai_label(Some(false), &[]).unwrap();
        assert_eq!(
            (tone, text.as_str()),
            (HealthTone::Warn, "Platené AI: vypnuté")
        );
        assert_eq!(
            tip,
            "Tento uzol nevolá platené AI (Gemini, Claude) — texty a metadáta berie medzitým \
             od susedného uzla"
        );
        let held: Vec<String> = ["dub", "lyrics", "metadata", "translation", "x"]
            .iter()
            .map(|k| k.to_string())
            .collect();
        let (_, _, tip) = paid_ai_label(Some(false), &held).unwrap();
        assert_eq!(
            tip,
            "Čaká, kým sa platené AI zapne: dabing, texty, metadáta, preklad, x (texty a \
             metadáta berie medzitým od susedného uzla)"
        );
    }
}
