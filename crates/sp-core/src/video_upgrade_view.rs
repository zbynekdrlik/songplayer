//! #223 S13: what Nastavenia "Video: sťahovanie a 4K" shows of the in-place
//! upgrade (`GET /api/v1/video-upgrade`) and of the download cap, in Slovak.
//! Shared by sp-ui and its tests; WASM-safe.

use serde::Deserialize;

/// The upgrade's status as Nastavenia reads it (a subset of the route's
/// answer; unknown fields are ignored).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct VideoUpgradeView {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub cap: u32,
    #[serde(default)]
    pub pending: i64,
    #[serde(default)]
    pub upgraded: i64,
    #[serde(default)]
    pub no_better: i64,
    #[serde(default)]
    pub refused: i64,
    #[serde(default)]
    pub failed: i64,
    #[serde(default)]
    pub busy: i64,
    #[serde(default)]
    pub rolled_back: i64,
    /// Why the worker's last tick ran no upgrade.
    #[serde(default)]
    pub waiting: Option<String>,
}

impl VideoUpgradeView {
    /// The status line: the three main counts, any other that is not zero,
    /// then why the worker waits.
    pub fn summary_sk(&self) -> String {
        let mut parts = vec![
            format!("vylepšené {}", self.upgraded),
            format!("bez vyššej kvality {}", self.no_better),
            format!("čaká {}", self.pending),
        ];
        for (count, label) in [
            (self.refused, "odmietnuté"),
            (self.failed, "chyba"),
            (self.busy, "obsadené prehrávaním"),
            (self.rolled_back, "vrátené späť"),
        ] {
            if count > 0 {
                parts.push(format!("{label} {count}"));
            }
        }
        let mut line = parts.join(" · ");
        if let Some(waiting) = &self.waiting {
            line.push_str(" — ");
            line.push_str(waiting_sk(waiting));
        }
        line
    }
}

/// Why the upgrade worker waits (the route's `waiting`), in Slovak; an
/// unknown reason as sent.
pub fn waiting_sk(waiting: &str) -> &str {
    match waiting {
        "off" => "vypnuté",
        "held" => "pozastavené pred bohoslužbou (sp-90s)",
        "download_due" => "najprv sťahuje nové piesne",
        "paused" => "pozastavené: YouTube žiada overenie",
        "spacing" => "medzi dvoma videami čaká 2 minúty",
        "low_disk" => "málo miesta na disku (pod 50 GiB)",
        "no_disk_reading" => "nevie zistiť voľné miesto na disku",
        "no_tools" => "čaká na nástroje (yt-dlp)",
        "nothing_to_do" => "všetky videá sú skontrolované",
        other => other,
    }
}

/// The label of a "Najvyššie rozlíšenie sťahovania" choice
/// (`config::MAX_RESOLUTION_CHOICES`, or a value set through the API).
pub fn cap_label(choice: &str) -> String {
    match choice {
        "" => "Automaticky (4K s dekódovaním na GPU, inak 1440)".to_string(),
        "2160" => "2160 (4K)".to_string(),
        "1440" | "1080" | "720" => choice.to_string(),
        other => format!("{other} (vlastné)"),
    }
}

#[cfg(test)]
#[path = "video_upgrade_view_tests.rs"]
mod tests;
