//! Pure mapping functions for the modern mixer UI (#181 D2).
//!
//! ONE mixer widget serves two domains: song stems (karaoke) and dub videos.
//! These functions translate between a domain's live control values (a karaoke
//! mode + vocal gain, or a dub mix ratio) and the mixer's fader/preset display,
//! with no I/O. They live in `sp-core` (WASM-safe) because sp-ui has no unit-test
//! job of its own — the workspace `Test` job (`cargo test --workspace`) covers
//! them here, and sp-ui re-exports them for the presentational component.

/// Which domain a mixer instance drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixerKind {
    /// Song stems: two faders (vokál / inštrumentál), karaoke-mode presets.
    Song,
    /// Dub video: three faders (originál hlas / dabing / ambient), ratio presets.
    Dub,
}

/// A selectable preset button: a stable wire id + a Slovak label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preset {
    /// Stable wire id (never shown to the operator).
    pub id: &'static str,
    /// Slovak button label.
    pub label: &'static str,
}

/// Song presets — ids match `sp_core::playback::KaraokeMode::as_str`.
pub const SONG_PRESETS: [Preset; 4] = [
    Preset {
        id: "full_mix",
        label: "Plný mix",
    },
    Preset {
        id: "karaoke_low",
        label: "Karaoke",
    },
    Preset {
        id: "vocals_only",
        label: "Iba vokály",
    },
    Preset {
        id: "instrumental_only",
        label: "Iba hudba",
    },
];

/// Dub presets — mix ratios 1.0 / 0.5 / 0.0.
pub const DUB_PRESETS: [Preset; 3] = [
    Preset {
        id: "dub_only",
        label: "Len dabing",
    },
    Preset {
        id: "half",
        label: "50 : 50",
    },
    Preset {
        id: "original",
        label: "Originál",
    },
];

/// Floor for the original bed under a dub WITHOUT stems (−18 dB), so the room
/// never goes dead. Mirrors the server's `stems::control::DUB_ORIGINAL_FLOOR`.
pub const DUB_ORIGINAL_FLOOR: f32 = 0.125;

/// The preset buttons for a mixer kind.
pub fn presets(kind: MixerKind) -> &'static [Preset] {
    match kind {
        MixerKind::Song => &SONG_PRESETS,
        MixerKind::Dub => &DUB_PRESETS,
    }
}

/// The channel (fader) labels for a mixer kind, in fader order.
pub fn channel_labels(kind: MixerKind) -> &'static [&'static str] {
    match kind {
        MixerKind::Song => &["vokál", "inštrumentál"],
        MixerKind::Dub => &["originál hlas", "dabing", "ambient"],
    }
}

/// The VISIBLE dub channel labels for a video with or without stems (#182). With
/// stems the full three-fader strip (`originál hlas` / `dabing` / `ambient`);
/// without stems the ambient bed does not exist (the 2-stream `DubOverOriginal`
/// mix), so only two faders are shown — the whole `originál` bed and `dabing`.
pub fn dub_channel_labels(has_stems: bool) -> &'static [&'static str] {
    if has_stems {
        &["originál hlas", "dabing", "ambient"]
    } else {
        &["originál", "dabing"]
    }
}

/// Song fader display gains `[vokál, inštrumentál]` for a karaoke preset.
/// `vocal_gain` (0..=1) only scales the Karaoke (`karaoke_low`) preset's vocals.
pub fn song_gains_for_preset(preset_id: &str, vocal_gain: f32) -> [f32; 2] {
    let vg = clamp01(vocal_gain);
    match preset_id {
        "karaoke_low" => [vg, 1.0],
        "vocals_only" => [1.0, 0.0],
        "instrumental_only" => [0.0, 1.0],
        // full_mix (default): the untouched original — both channels full.
        _ => [1.0, 1.0],
    }
}

/// Inverse of [`song_gains_for_preset`]: the karaoke-mode id best matching a pair
/// of `[vokál, inštrumentál]` fader gains, or `None` if nothing matches.
pub fn song_preset_for_gains(gains: [f32; 2]) -> Option<&'static str> {
    match (permille(gains[0]), permille(gains[1])) {
        (1000, 1000) => Some("full_mix"),
        (1000, 0) => Some("vocals_only"),
        (0, 1000) => Some("instrumental_only"),
        // Vocals reduced below full while the instrumental is full → Karaoke.
        (_, 1000) => Some("karaoke_low"),
        _ => None,
    }
}

/// Dub fader display gains `[originál hlas, dabing, ambient]` for a mix ratio `r`.
/// With stems the original *voice* is `1−r`; without stems the original *bed* is
/// floored at [`DUB_ORIGINAL_FLOOR`]. Ambient is a fixed reference (`1.0`).
pub fn ratio_to_faders(r: f32, has_stems: bool) -> Vec<f32> {
    let r = clamp01(r);
    let orig = if has_stems {
        1.0 - r
    } else {
        (1.0 - r).max(DUB_ORIGINAL_FLOOR)
    };
    vec![orig, r, 1.0]
}

/// Inverse of [`ratio_to_faders`]: the dub mix ratio is the `dabing` channel
/// (index 1) — the only fader the operator drives.
pub fn faders_to_ratio(faders: &[f32]) -> f32 {
    clamp01(faders.get(1).copied().unwrap_or(1.0))
}

/// The mix ratio a dub preset selects.
pub fn dub_ratio_for_preset(preset_id: &str) -> f32 {
    match preset_id {
        "original" => 0.0,
        "half" => 0.5,
        // dub_only (default).
        _ => 1.0,
    }
}

/// Inverse: the dub preset id best matching a ratio, or `None` between points.
pub fn dub_preset_for_ratio(r: f32) -> Option<&'static str> {
    match permille(r) {
        1000 => Some("dub_only"),
        500 => Some("half"),
        0 => Some("original"),
        _ => None,
    }
}

/// Unified: fader display gains for a preset of the given kind.
pub fn gains_for_preset(kind: MixerKind, preset_id: &str, vocal_gain: f32) -> Vec<f32> {
    match kind {
        MixerKind::Song => song_gains_for_preset(preset_id, vocal_gain).to_vec(),
        MixerKind::Dub => ratio_to_faders(dub_ratio_for_preset(preset_id), true),
    }
}

/// Unified: the preset id best matching a set of fader gains, or `None`.
pub fn preset_for_gains(kind: MixerKind, gains: &[f32]) -> Option<&'static str> {
    match kind {
        MixerKind::Song => {
            if gains.len() >= 2 {
                song_preset_for_gains([gains[0], gains[1]])
            } else {
                None
            }
        }
        MixerKind::Dub => dub_preset_for_ratio(faders_to_ratio(gains)),
    }
}

/// The percent a mixer fader should DISPLAY (and bind to `prop:value`): while
/// the operator is dragging the fader the dragged value is authoritative, so a
/// live gain update from the store (an adapter `Effect`, a re-load) can't
/// overwrite the fader out from under the finger; otherwise the live gain
/// percent drives it. #194 — the fader half of the seek drag gate; pure so its
/// boundary is unit-tested (sp-ui has no unit-test job).
pub fn fader_display_pct(dragging: bool, dragged_pct: i32, live_pct: i32) -> i32 {
    if dragging { dragged_pct } else { live_pct }
}

/// Clamp a gain/ratio to `0.0..=1.0`, mapping NaN to `0.0` (never propagate NaN
/// into the DOM or the mix).
fn clamp01(x: f32) -> f32 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

/// Quantise a `0..=1` gain to integer permille (`0..=1000`) for exact preset
/// matching that tolerates the f32 rounding of the UI's integer-percent faders.
fn permille(x: f32) -> i32 {
    (clamp01(x) * 1000.0).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Preset / label tables ────────────────────────────────────────────────

    #[test]
    fn song_presets_are_the_four_karaoke_modes_in_order() {
        let ids: Vec<&str> = presets(MixerKind::Song).iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            [
                "full_mix",
                "karaoke_low",
                "vocals_only",
                "instrumental_only"
            ]
        );
    }

    #[test]
    fn dub_presets_are_the_three_ratio_points_in_order() {
        let ids: Vec<&str> = presets(MixerKind::Dub).iter().map(|p| p.id).collect();
        assert_eq!(ids, ["dub_only", "half", "original"]);
    }

    #[test]
    fn channel_labels_differ_by_kind() {
        assert_eq!(channel_labels(MixerKind::Song), ["vokál", "inštrumentál"]);
        assert_eq!(
            channel_labels(MixerKind::Dub),
            ["originál hlas", "dabing", "ambient"]
        );
    }

    #[test]
    fn dub_channels_hide_ambient_without_stems() {
        // With stems: the full three-fader strip.
        assert_eq!(
            dub_channel_labels(true),
            ["originál hlas", "dabing", "ambient"]
        );
        // Without stems: only two faders — ambient is hidden.
        assert_eq!(dub_channel_labels(false), ["originál", "dabing"]);
        assert_eq!(dub_channel_labels(false).len(), 2);
        assert_eq!(dub_channel_labels(true).len(), 3);
    }

    // ── Song preset → fader gains ─────────────────────────────────────────────

    #[test]
    fn song_full_mix_is_both_channels_full() {
        assert_eq!(song_gains_for_preset("full_mix", 0.3), [1.0, 1.0]);
    }

    #[test]
    fn song_karaoke_low_scales_only_the_vocal_channel() {
        assert_eq!(song_gains_for_preset("karaoke_low", 0.2), [0.2, 1.0]);
        assert_eq!(song_gains_for_preset("karaoke_low", 0.8), [0.8, 1.0]);
    }

    #[test]
    fn song_vocals_only_mutes_the_instrumental() {
        assert_eq!(song_gains_for_preset("vocals_only", 0.5), [1.0, 0.0]);
    }

    #[test]
    fn song_instrumental_only_mutes_the_vocal() {
        assert_eq!(song_gains_for_preset("instrumental_only", 0.5), [0.0, 1.0]);
    }

    #[test]
    fn song_unknown_preset_defaults_to_full_mix() {
        assert_eq!(song_gains_for_preset("bogus", 0.4), [1.0, 1.0]);
    }

    #[test]
    fn song_vocal_gain_is_clamped_into_unit_range() {
        assert_eq!(song_gains_for_preset("karaoke_low", 1.5), [1.0, 1.0]);
        assert_eq!(song_gains_for_preset("karaoke_low", -0.5), [0.0, 1.0]);
    }

    #[test]
    fn song_vocal_gain_nan_clamps_to_zero_not_nan() {
        let [v, i] = song_gains_for_preset("karaoke_low", f32::NAN);
        assert_eq!(v, 0.0);
        assert_eq!(i, 1.0);
    }

    // ── Song fader gains → preset (round-trip) ────────────────────────────────

    #[test]
    fn song_preset_for_gains_round_trips_every_preset() {
        assert_eq!(song_preset_for_gains([1.0, 1.0]), Some("full_mix"));
        assert_eq!(song_preset_for_gains([1.0, 0.0]), Some("vocals_only"));
        assert_eq!(song_preset_for_gains([0.0, 1.0]), Some("instrumental_only"));
        // Vocals reduced below full with the instrumental still full → Karaoke.
        assert_eq!(song_preset_for_gains([0.3, 1.0]), Some("karaoke_low"));
    }

    #[test]
    fn song_preset_for_gains_is_none_when_nothing_matches() {
        // Both channels partially down: not any named preset.
        assert_eq!(song_preset_for_gains([0.5, 0.5]), None);
        assert_eq!(song_preset_for_gains([0.0, 0.0]), None);
    }

    // ── Dub ratio → faders ────────────────────────────────────────────────────

    #[test]
    fn dub_faders_with_stems_split_original_and_dub() {
        assert_eq!(ratio_to_faders(0.3, true), vec![0.7, 0.3, 1.0]);
        assert_eq!(ratio_to_faders(1.0, true), vec![0.0, 1.0, 1.0]);
        assert_eq!(ratio_to_faders(0.0, true), vec![1.0, 0.0, 1.0]);
    }

    #[test]
    fn dub_faders_without_stems_floor_the_original_bed() {
        // r = 0.9375 → 1 − r = 0.0625 (both f32-exact) < floor, so WITHOUT stems
        // the bed holds at DUB_ORIGINAL_FLOOR (0.125)...
        assert_eq!(ratio_to_faders(0.9375, false), vec![0.125, 0.9375, 1.0]);
        // ...and WITH stems the same ratio is NOT floored (proves the branch).
        assert_eq!(ratio_to_faders(0.9375, true), vec![0.0625, 0.9375, 1.0]);
    }

    #[test]
    fn dub_ratio_is_clamped_into_unit_range() {
        assert_eq!(ratio_to_faders(2.0, true), vec![0.0, 1.0, 1.0]);
        assert_eq!(ratio_to_faders(-1.0, true), vec![1.0, 0.0, 1.0]);
    }

    // ── Dub faders → ratio (round-trip) ───────────────────────────────────────

    #[test]
    fn dub_faders_to_ratio_reads_the_dabing_channel() {
        assert_eq!(faders_to_ratio(&[0.2, 0.7, 1.0]), 0.7);
        // Round-trip through ratio_to_faders.
        assert_eq!(faders_to_ratio(&ratio_to_faders(0.42, true)), 0.42);
    }

    #[test]
    fn dub_faders_to_ratio_defaults_to_full_dub_when_empty() {
        assert_eq!(faders_to_ratio(&[]), 1.0);
    }

    // ── Dub preset ↔ ratio ────────────────────────────────────────────────────

    #[test]
    fn dub_ratio_for_preset_maps_each_button() {
        assert_eq!(dub_ratio_for_preset("dub_only"), 1.0);
        assert_eq!(dub_ratio_for_preset("half"), 0.5);
        assert_eq!(dub_ratio_for_preset("original"), 0.0);
        assert_eq!(dub_ratio_for_preset("bogus"), 1.0); // default = dub only
    }

    #[test]
    fn dub_preset_for_ratio_round_trips_each_point() {
        assert_eq!(dub_preset_for_ratio(1.0), Some("dub_only"));
        assert_eq!(dub_preset_for_ratio(0.5), Some("half"));
        assert_eq!(dub_preset_for_ratio(0.0), Some("original"));
        assert_eq!(dub_preset_for_ratio(0.25), None);
    }

    // ── Unified wrappers ──────────────────────────────────────────────────────

    #[test]
    fn unified_gains_for_preset_dispatches_by_kind() {
        assert_eq!(
            gains_for_preset(MixerKind::Song, "karaoke_low", 0.2),
            vec![0.2, 1.0]
        );
        assert_eq!(
            gains_for_preset(MixerKind::Dub, "half", 0.0),
            vec![0.5, 0.5, 1.0]
        );
    }

    #[test]
    fn unified_preset_for_gains_dispatches_by_kind() {
        assert_eq!(
            preset_for_gains(MixerKind::Song, &[1.0, 0.0]),
            Some("vocals_only")
        );
        assert_eq!(
            preset_for_gains(MixerKind::Dub, &[0.0, 1.0, 1.0]),
            Some("dub_only")
        );
        // Too few song faders → None rather than a panic.
        assert_eq!(preset_for_gains(MixerKind::Song, &[1.0]), None);
    }

    // ── fader_display_pct: dragged while dragging, else live ──────────────────

    #[test]
    fn fader_display_dragging_returns_the_dragged_pct() {
        // dragged != live so this also kills a "return live" mutant.
        assert_eq!(fader_display_pct(true, 40, 100), 40);
    }

    #[test]
    fn fader_display_not_dragging_returns_the_live_pct() {
        // dragged != live so this also kills a "return dragged" mutant.
        assert_eq!(fader_display_pct(false, 40, 100), 100);
    }
}
