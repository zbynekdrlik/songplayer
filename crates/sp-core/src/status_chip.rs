//! #194 ROUND 2: ONE status vocabulary for every song row in the app.
//!
//! The Dashboard list, the Live set list + catalog, the Lyrics list and the
//! Dabing list all describe the same four per-song capabilities — stems, text
//! (lyrics), dabing and the downloaded/normalized file. Before #194 each page
//! had its own glyphs and words for the same wire values (two stems
//! vocabularies, three boolean-glyph systems, English mixed with Slovak). This
//! module is the single source of truth: pure, WASM-safe builders that map the
//! wire fields to a `StatusChip { kind, label_sk, tone }`, so the SAME Slovak
//! chip renders on every page.
//!
//! It lives in `sp_core` (not sp-ui) because sp-ui has no unit-test job — the
//! label/tone mapping is covered here by the workspace `Test` job and the
//! diff-scoped mutation gate, and sp-ui renders whatever these return.

/// Which per-song capability a chip describes. Drives the per-chip test id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Stems,
    Text,
    Dub,
    File,
}

impl ChipKind {
    /// Stable `data-testid` for this chip, identical on every page.
    pub fn testid(self) -> &'static str {
        match self {
            ChipKind::Stems => "chip-stems",
            ChipKind::Text => "chip-text",
            ChipKind::Dub => "chip-dub",
            ChipKind::File => "chip-file",
        }
    }
}

/// Colour family for a chip. sp-ui maps each to one CSS class, so the same
/// state has the same colour on every page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipTone {
    /// Done / verified — green.
    Ok,
    /// Actively processing — blue.
    Progress,
    /// Waiting in a queue — amber.
    Queued,
    /// Absent / not applicable — grey.
    Missing,
    /// Failed — red.
    Error,
}

impl ChipTone {
    /// CSS class for this tone (one colour per state, everywhere).
    pub fn css_class(self) -> &'static str {
        match self {
            ChipTone::Ok => "chip-ok",
            ChipTone::Progress => "chip-progress",
            ChipTone::Queued => "chip-queued",
            ChipTone::Missing => "chip-missing",
            ChipTone::Error => "chip-error",
        }
    }
}

/// One rendered status chip: what it is, its Slovak label, and its colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusChip {
    pub kind: ChipKind,
    pub label_sk: String,
    pub tone: ChipTone,
}

impl StatusChip {
    fn new(kind: ChipKind, label_sk: impl Into<String>, tone: ChipTone) -> Self {
        Self {
            kind,
            label_sk: label_sk.into(),
            tone,
        }
    }
}

/// Stems chip from the per-video `stems_state` wire value
/// (`ready` / `processing` / `queued` / `unavailable` / `failed`) plus an
/// optional queue position for the `queued` state. Any unknown or absent state
/// reads as `nedostupné` — the row never shows a bare/blank stems cell.
pub fn stems_chip(state: Option<&str>, queue_pos: Option<u32>) -> StatusChip {
    let (label, tone) = match state {
        Some("ready") => ("hotové".to_string(), ChipTone::Ok),
        Some("processing") => ("spracúva sa".to_string(), ChipTone::Progress),
        Some("queued") => {
            let label = match queue_pos {
                Some(n) => format!("vo fronte ({n}.)"),
                None => "vo fronte".to_string(),
            };
            (label, ChipTone::Queued)
        }
        Some("failed") => ("chyba".to_string(), ChipTone::Error),
        // "unavailable", None, or any unknown value → not applicable.
        _ => ("nedostupné".to_string(), ChipTone::Missing),
    };
    StatusChip::new(ChipKind::Stems, label, tone)
}

/// Text (lyrics) chip from the flags a payload carries: whether lyrics exist,
/// whether they are the verified `★` reference, and whether they are stale
/// (produced under an older pipeline version). A missing text always wins, then
/// the verified reference, then stale, else base-tier lyrics.
pub fn text_chip(has_lyrics: bool, is_reference: bool, is_stale: bool) -> StatusChip {
    let (label, tone) = if !has_lyrics {
        ("chýba".to_string(), ChipTone::Missing)
    } else if is_reference {
        ("★ overený".to_string(), ChipTone::Ok)
    } else if is_stale {
        ("zastaraný".to_string(), ChipTone::Queued)
    } else {
        ("základný".to_string(), ChipTone::Progress)
    };
    StatusChip::new(ChipKind::Text, label, tone)
}

/// Dabing chip from the dub chain wire value (`dub_status` on a Video, or
/// `chain_state` on a DubRow): `ready` / `failed` / `queued` / `none` /
/// `stems` / `transcript` / `translation` / `synth`. Anything in the middle of
/// the chain reads as `beží`; `none` / absent reads as `—`.
pub fn dub_chip(status: Option<&str>) -> StatusChip {
    let (label, tone) = match status {
        Some("ready") => ("hotový".to_string(), ChipTone::Ok),
        Some("failed") => ("chyba".to_string(), ChipTone::Error),
        Some("queued") => ("vo fronte".to_string(), ChipTone::Queued),
        Some("none") | Some("") | None => ("—".to_string(), ChipTone::Missing),
        // stems / transcript / translation / synth — mid-chain.
        Some(_) => ("beží".to_string(), ChipTone::Progress),
    };
    StatusChip::new(ChipKind::Dub, label, tone)
}

/// The downloaded/normalized file as ONE chip. A fully normalized song reads
/// `stiahnuté`; a song downloaded but not yet normalized reads `sťahuje sa`;
/// nothing on disk reads `chýba`.
pub fn file_chip(cached: bool, normalized: bool) -> StatusChip {
    let (label, tone) = if normalized {
        ("stiahnuté".to_string(), ChipTone::Ok)
    } else if cached {
        ("sťahuje sa".to_string(), ChipTone::Progress)
    } else {
        ("chýba".to_string(), ChipTone::Missing)
    };
    StatusChip::new(ChipKind::File, label, tone)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- stems_chip ----

    #[test]
    fn stems_ready() {
        let c = stems_chip(Some("ready"), None);
        assert_eq!(c.kind, ChipKind::Stems);
        assert_eq!(c.label_sk, "hotové");
        assert_eq!(c.tone, ChipTone::Ok);
    }

    #[test]
    fn stems_processing() {
        let c = stems_chip(Some("processing"), None);
        assert_eq!(c.label_sk, "spracúva sa");
        assert_eq!(c.tone, ChipTone::Progress);
    }

    #[test]
    fn stems_queued_no_position() {
        let c = stems_chip(Some("queued"), None);
        assert_eq!(c.label_sk, "vo fronte");
        assert_eq!(c.tone, ChipTone::Queued);
    }

    #[test]
    fn stems_queued_with_position() {
        let c = stems_chip(Some("queued"), Some(3));
        assert_eq!(c.label_sk, "vo fronte (3.)");
        assert_eq!(c.tone, ChipTone::Queued);
    }

    #[test]
    fn stems_queued_position_one_boundary() {
        assert_eq!(
            stems_chip(Some("queued"), Some(1)).label_sk,
            "vo fronte (1.)"
        );
    }

    #[test]
    fn stems_failed() {
        let c = stems_chip(Some("failed"), None);
        assert_eq!(c.label_sk, "chyba");
        assert_eq!(c.tone, ChipTone::Error);
    }

    #[test]
    fn stems_unavailable() {
        let c = stems_chip(Some("unavailable"), None);
        assert_eq!(c.label_sk, "nedostupné");
        assert_eq!(c.tone, ChipTone::Missing);
    }

    #[test]
    fn stems_none() {
        assert_eq!(stems_chip(None, None).label_sk, "nedostupné");
        assert_eq!(stems_chip(None, None).tone, ChipTone::Missing);
    }

    #[test]
    fn stems_unknown_value() {
        assert_eq!(stems_chip(Some("weird"), None).label_sk, "nedostupné");
    }

    // ---- text_chip ----

    #[test]
    fn text_missing() {
        let c = text_chip(false, false, false);
        assert_eq!(c.kind, ChipKind::Text);
        assert_eq!(c.label_sk, "chýba");
        assert_eq!(c.tone, ChipTone::Missing);
    }

    #[test]
    fn text_missing_beats_reference_and_stale() {
        // No lyrics at all wins even if the other flags are set.
        assert_eq!(text_chip(false, true, true).label_sk, "chýba");
    }

    #[test]
    fn text_reference() {
        let c = text_chip(true, true, false);
        assert_eq!(c.label_sk, "★ overený");
        assert_eq!(c.tone, ChipTone::Ok);
    }

    #[test]
    fn text_reference_beats_stale() {
        assert_eq!(text_chip(true, true, true).label_sk, "★ overený");
    }

    #[test]
    fn text_stale() {
        let c = text_chip(true, false, true);
        assert_eq!(c.label_sk, "zastaraný");
        assert_eq!(c.tone, ChipTone::Queued);
    }

    #[test]
    fn text_base() {
        let c = text_chip(true, false, false);
        assert_eq!(c.label_sk, "základný");
        assert_eq!(c.tone, ChipTone::Progress);
    }

    // ---- dub_chip ----

    #[test]
    fn dub_ready() {
        let c = dub_chip(Some("ready"));
        assert_eq!(c.kind, ChipKind::Dub);
        assert_eq!(c.label_sk, "hotový");
        assert_eq!(c.tone, ChipTone::Ok);
    }

    #[test]
    fn dub_failed() {
        let c = dub_chip(Some("failed"));
        assert_eq!(c.label_sk, "chyba");
        assert_eq!(c.tone, ChipTone::Error);
    }

    #[test]
    fn dub_queued() {
        let c = dub_chip(Some("queued"));
        assert_eq!(c.label_sk, "vo fronte");
        assert_eq!(c.tone, ChipTone::Queued);
    }

    #[test]
    fn dub_none() {
        assert_eq!(dub_chip(Some("none")).label_sk, "—");
        assert_eq!(dub_chip(Some("none")).tone, ChipTone::Missing);
    }

    #[test]
    fn dub_absent() {
        assert_eq!(dub_chip(None).label_sk, "—");
        assert_eq!(dub_chip(Some("")).label_sk, "—");
    }

    #[test]
    fn dub_mid_chain_stems() {
        let c = dub_chip(Some("stems"));
        assert_eq!(c.label_sk, "beží");
        assert_eq!(c.tone, ChipTone::Progress);
    }

    #[test]
    fn dub_mid_chain_all_stages() {
        for s in ["stems", "transcript", "translation", "synth"] {
            assert_eq!(dub_chip(Some(s)).label_sk, "beží", "stage {s}");
        }
    }

    // ---- file_chip ----

    #[test]
    fn file_normalized() {
        let c = file_chip(true, true);
        assert_eq!(c.kind, ChipKind::File);
        assert_eq!(c.label_sk, "stiahnuté");
        assert_eq!(c.tone, ChipTone::Ok);
    }

    #[test]
    fn file_cached_only() {
        let c = file_chip(true, false);
        assert_eq!(c.label_sk, "sťahuje sa");
        assert_eq!(c.tone, ChipTone::Progress);
    }

    #[test]
    fn file_missing() {
        let c = file_chip(false, false);
        assert_eq!(c.label_sk, "chýba");
        assert_eq!(c.tone, ChipTone::Missing);
    }

    #[test]
    fn file_normalized_without_cached_flag_still_done() {
        // normalized wins regardless of the cached flag.
        assert_eq!(file_chip(false, true).label_sk, "stiahnuté");
        assert_eq!(file_chip(false, true).tone, ChipTone::Ok);
    }

    // ---- testid / css_class vocabulary ----

    #[test]
    fn chip_kind_testids() {
        assert_eq!(ChipKind::Stems.testid(), "chip-stems");
        assert_eq!(ChipKind::Text.testid(), "chip-text");
        assert_eq!(ChipKind::Dub.testid(), "chip-dub");
        assert_eq!(ChipKind::File.testid(), "chip-file");
    }

    #[test]
    fn chip_tone_css_classes() {
        assert_eq!(ChipTone::Ok.css_class(), "chip-ok");
        assert_eq!(ChipTone::Progress.css_class(), "chip-progress");
        assert_eq!(ChipTone::Queued.css_class(), "chip-queued");
        assert_eq!(ChipTone::Missing.css_class(), "chip-missing");
        assert_eq!(ChipTone::Error.css_class(), "chip-error");
    }
}
