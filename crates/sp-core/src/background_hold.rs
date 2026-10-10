//! #230 — the owner's rule (6.10.2026), verbatim: "ked sa spusti scena v
//! streamdecku 5min tak sa blokne dalsie spracovanie/stahovanie na pozadi a
//! odblokuje sa automaticky ked sa spusti scena yt slov alebo prejdu 4hod";
//! ruled 10.10.2026: "zatial daj sp90s co je aj playlist aj scena v
//! streamdecku" — the press is the `yt90s` playlist's scene (~1.5 min before
//! the service), the release the `ytslow` playlist's scene.
//!
//! A press of [`HOLD_SCENE`] holds every background job for [`HOLD_FOR_S`]
//! (a re-press re-arms it); a press of [`RELEASE_SCENE`], or the time, ends
//! it. The hold is the stored end instant, [`SETTING_BACKGROUND_HOLD_UNTIL`]
//! (UTC unix ms): held while now is before it. sp-server's
//! `background_hold` arms it from the program bus and every worker asks it
//! before it starts a job; a running job finishes. Here: the pure decisions
//! and the health bar's Slovak segment, shared with sp-ui.

use serde::{Deserialize, Serialize};

use crate::health::HealthTone;

/// The scene whose press holds the background work: `yt90s`'s (its NDI
/// name `SP-90s`, lowercased as the scene catalog names it).
pub const HOLD_SCENE: &str = "sp-90s";

/// The scene whose press ends the hold: `ytslow`'s (`SP-slow`).
pub const RELEASE_SCENE: &str = "sp-slow";

/// How long a press holds when nothing releases it: 4 h. A literal: a
/// `4 * 3600` would list two mutants nothing could tell apart.
pub const HOLD_FOR_S: u64 = 14_400;

/// The setting holding the hold's end, UTC unix ms; absent = not held.
pub const SETTING_BACKGROUND_HOLD_UNTIL: &str = "background_hold_until_ms";

/// What a cut to a scene does to the hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// [`HOLD_SCENE`]: hold for [`HOLD_FOR_S`] from now.
    Hold,
    /// [`RELEASE_SCENE`]: end the hold.
    Release,
    /// Any other scene, or none: nothing.
    Other,
}

/// What a cut to `scene` does (ASCII case ignored, as the catalog does).
pub fn press_of(scene: Option<&str>) -> Press {
    match scene.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case(HOLD_SCENE) => Press::Hold,
        Some(s) if s.eq_ignore_ascii_case(RELEASE_SCENE) => Press::Release,
        _ => Press::Other,
    }
}

/// The stored end instant: a positive whole number of ms, else none (a
/// mangled value holds nothing).
pub fn parse_until(raw: Option<&str>) -> Option<i64> {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|ms| *ms > 0)
}

/// Whether a hold ending at `until_ms` holds at `now_ms` (the end itself is
/// free).
pub fn held_at(until_ms: Option<i64>, now_ms: i64) -> bool {
    until_ms.is_some_and(|until| now_ms < until)
}

/// Whole seconds left of the hold at `now_ms`, rounded up; 0 when not held.
/// No comparison of its own (at the end itself the time left is 0 either
/// way, so a `<` against `<=` there would be a mutant nothing can see).
pub fn remaining_s(until_ms: Option<i64>, now_ms: i64) -> u64 {
    let left_ms = until_ms.map_or(0, |until| until.saturating_sub(now_ms));
    u64::try_from(left_ms).unwrap_or(0).div_ceil(1000)
}

/// The hold as `GET /api/v1/background-hold` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundHold {
    /// Whether background jobs are held now.
    pub held: bool,
    /// The stored end, UTC unix ms (kept while it is stored, past or not).
    pub until_utc_ms: Option<i64>,
    /// Seconds left while held ([`remaining_s`]), else 0.
    pub remaining_s: u64,
    /// [`HOLD_SCENE`].
    pub hold_scene: String,
    /// [`RELEASE_SCENE`].
    pub release_scene: String,
    /// The jobs that found the hold since it was armed (sp-server's
    /// `Job::as_str`), in order; empty while not held.
    pub held_jobs: Vec<String>,
}

/// The health bar's segment, only while held: amber `Pozadie: pozastavené
/// (ešte 3 h 58 min)`, the tooltip naming the trigger, the release and the
/// work that waits.
pub fn label(hold: &BackgroundHold) -> Option<(HealthTone, String, String)> {
    if !hold.held {
        return None;
    }
    let text = format!(
        "Pozadie: pozastavené (ešte {})",
        duration_sk(hold.remaining_s)
    );
    let waits = if hold.held_jobs.is_empty() {
        "Zatiaľ nič nečaká.".to_string()
    } else {
        format!(
            "Čaká: {}.",
            hold.held_jobs
                .iter()
                .map(|j| job_sk(j.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let tip = format!(
        "Na programe bola scéna {}: nové sťahovanie a spracovanie na pozadí sa nespustí, \
         kým na program nepôjde {} alebo neuplynú {} h (rozbehnutá práca dobehne). {waits}",
        hold.hold_scene,
        hold.release_scene,
        HOLD_FOR_S / 3600
    );
    Some((HealthTone::Warn, text, tip))
}

/// `seconds` in whole minutes, rounded up: `3 h 58 min`, `4 h`, `12 min`.
fn duration_sk(seconds: u64) -> String {
    let minutes = seconds.div_ceil(60);
    let (h, m) = (minutes / 60, minutes % 60);
    match (h, m) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// A held job (sp-server's `Job::as_str`) in Slovak; an unknown one as sent.
fn job_sk(job: &str) -> &str {
    match job {
        "sync" => "synchronizácia playlistov",
        "download" => "sťahovanie",
        "lyrics" => "texty",
        "stems" => "stopy",
        "dub" => "dabing",
        "metadata" => "oprava názvov",
        "peer" => "výmena so susedným uzlom",
        "ytdlp_update" => "aktualizácia yt-dlp",
        other => other,
    }
}

#[cfg(test)]
#[path = "background_hold_tests.rs"]
mod tests;
