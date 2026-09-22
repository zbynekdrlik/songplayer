//! Pure fader/preset model for the ONE mixer (#184 round G).
//!
//! The mixer is FADER-shaped, not preset-shaped: three independent faders
//! `[vokály, podklad, dabing]` ARE the state, and the per-stream mix gains are
//! DERIVED from them (`stream_gains_*`). Presets are just fader snapshots. This
//! module is pure (no I/O), WASM-safe, and unit-tested in the workspace — sp-ui
//! has no unit-test job of its own, so this is where the exact-value behaviour is
//! pinned and mutation-gated. sp-ui re-exports these for the `LiveMixer` adapter,
//! and the server derives its live gain atomics from the SAME `stream_gains_*`.

/// The three live faders, each `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixFaders {
    /// The vocal / original-voice fader (with stems: the vocals stem; a no-stems
    /// dub: the whole original bed).
    pub vokaly: f32,
    /// The instrumental / backing-track fader (needs stems).
    pub podklad: f32,
    /// The Slovak dub fader (only meaningful for a dub video).
    pub dabing: f32,
}

impl Default for MixFaders {
    /// The safe default: everything full — the bit-exact original mix, dub full.
    fn default() -> Self {
        Self {
            vokaly: 1.0,
            podklad: 1.0,
            dabing: 1.0,
        }
    }
}

impl MixFaders {
    /// Build clamped faders. If ANY component is non-finite the WHOLE triple falls
    /// back to the default `(1, 1, 1)` — never propagate a NaN into the mix gains
    /// or the DOM.
    pub fn new(vokaly: f32, podklad: f32, dabing: f32) -> Self {
        if !vokaly.is_finite() || !podklad.is_finite() || !dabing.is_finite() {
            return Self::default();
        }
        Self {
            vokaly: vokaly.clamp(0.0, 1.0),
            podklad: podklad.clamp(0.0, 1.0),
            dabing: dabing.clamp(0.0, 1.0),
        }
    }
}

/// Per-stream gains `[original, vocals, instrumental]` for a SONG (both stems
/// present). When BOTH `vokály` and `podklad` are full the ORIGINAL mix plays
/// bit-exact (`[1, 0, 0]` — no separation artefacts, today's FullMix); otherwise
/// the two stems are mixed at the fader positions and the original is silent so
/// the voices are never doubled.
pub fn stream_gains_song(f: MixFaders) -> [f32; 3] {
    if f.vokaly == 1.0 && f.podklad == 1.0 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, f.vokaly, f.podklad]
    }
}

/// Per-stream gains `[original, vocals, instrumental, dub]` for a dub video WITH
/// both stems. Same bit-exact-original rule for the voice/bed pair as
/// [`stream_gains_song`]; the `dabing` fader is ALWAYS the dub-stream gain, so the
/// Slovak dub is domixed over whatever the original pair plays.
pub fn stream_gains_dub(f: MixFaders) -> [f32; 4] {
    if f.vokaly == 1.0 && f.podklad == 1.0 {
        [1.0, 0.0, 0.0, f.dabing]
    } else {
        [0.0, f.vokaly, f.podklad, f.dabing]
    }
}

/// Per-stream gains `[original, dub]` for a dub video WITHOUT stems (not yet
/// separated). There is no split, so the `vokály` fader IS the whole original bed
/// (no floor — "stiahnuť originál na 0" = vokály 0) and `dabing` is the dub.
pub fn stream_gains_dub_no_stems(f: MixFaders) -> [f32; 2] {
    [f.vokaly, f.dabing]
}

/// A preset button: a stable wire id (never shown), a Slovak label, the
/// `vokály`/`podklad` snapshot, and — for a DUB preset — a pinned `dabing`. A song
/// preset leaves `dabing` untouched (`dabing: None`); a dub preset pins all three.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    /// Stable wire id (matches the `mixer-preset-<id>` test id).
    pub id: &'static str,
    /// Slovak button label.
    pub label: &'static str,
    /// The `vokály` snapshot.
    pub vokaly: f32,
    /// The `podklad` snapshot.
    pub podklad: f32,
    /// `Some(d)` pins the `dabing` fader (dub presets); `None` leaves it untouched
    /// (song presets — `·` in the design).
    pub dabing: Option<f32>,
}

/// The four SONG presets (always shown). `dabing: None` = leave the dub fader.
pub const SONG_PRESETS: [Preset; 4] = [
    Preset {
        id: "full_mix",
        label: "Plný mix",
        vokaly: 1.0,
        podklad: 1.0,
        dabing: None,
    },
    Preset {
        id: "karaoke_low",
        label: "Karaoke",
        vokaly: 0.3,
        podklad: 1.0,
        dabing: None,
    },
    Preset {
        id: "vocals_only",
        label: "Iba vokály",
        vokaly: 1.0,
        podklad: 0.0,
        dabing: None,
    },
    Preset {
        id: "instrumental_only",
        label: "Iba hudba",
        vokaly: 0.0,
        podklad: 1.0,
        dabing: None,
    },
];

/// The three DUB presets (shown only when the playing item has a ready dub). Each
/// pins all three faders.
pub const DUB_PRESETS: [Preset; 3] = [
    Preset {
        id: "dub_only",
        label: "Len dabing",
        vokaly: 0.0,
        podklad: 1.0,
        dabing: Some(1.0),
    },
    Preset {
        id: "half",
        label: "50 : 50",
        vokaly: 0.5,
        podklad: 1.0,
        dabing: Some(0.5),
    },
    Preset {
        id: "original",
        label: "Originál",
        vokaly: 1.0,
        podklad: 1.0,
        dabing: Some(0.0),
    },
];

/// The preset buttons to show: always the four song presets, plus the three dub
/// presets when the playing item has a ready dub.
pub fn presets(has_dub: bool) -> Vec<Preset> {
    let mut v = SONG_PRESETS.to_vec();
    if has_dub {
        v.extend_from_slice(&DUB_PRESETS);
    }
    v
}

/// Apply a preset snapshot on top of the current faders: `vokály`/`podklad` come
/// from the preset; `dabing` from the preset when it pins one (dub presets), else
/// the CURRENT dabing is kept (song presets leave the dub fader untouched).
pub fn apply_preset(preset: &Preset, current: MixFaders) -> MixFaders {
    MixFaders::new(
        preset.vokaly,
        preset.podklad,
        preset.dabing.unwrap_or(current.dabing),
    )
}

/// Matching tolerance for the preset highlight — the UI's integer-percent faders
/// round, so a snapshot matches "within 0.01".
const PRESET_TOL: f32 = 0.01;

fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() <= PRESET_TOL
}

/// The preset id whose snapshot matches the current faders, or `None` (a custom
/// mix). A dub preset (which pins all three) is the MORE SPECIFIC match, so when a
/// dub is present those are checked first; otherwise the song presets match on
/// `[vokály, podklad]` alone (`dabing` ignored — `karaoke_low`: `vokály < 1 &&
/// podklad == 1`).
pub fn preset_for_faders(f: MixFaders, has_dub: bool) -> Option<&'static str> {
    if has_dub {
        for p in &DUB_PRESETS {
            if approx(f.vokaly, p.vokaly)
                && approx(f.podklad, p.podklad)
                && approx(f.dabing, p.dabing.unwrap_or(1.0))
            {
                return Some(p.id);
            }
        }
    }
    song_preset_for_faders(f.vokaly, f.podklad)
}

/// The song preset id for a `[vokály, podklad]` pair (dabing ignored). The
/// instrumental (`podklad`) must be full; then `vokály` picks the preset —
/// full → `full_mix`, muted → `instrumental_only`, in-between → `karaoke_low`;
/// or `vocals_only` when the instrumental is muted and vocals full.
fn song_preset_for_faders(vokaly: f32, podklad: f32) -> Option<&'static str> {
    if approx(podklad, 1.0) {
        if approx(vokaly, 1.0) {
            Some("full_mix")
        } else if approx(vokaly, 0.0) {
            Some("instrumental_only")
        } else {
            Some("karaoke_low")
        }
    } else if approx(podklad, 0.0) && approx(vokaly, 1.0) {
        Some("vocals_only")
    } else {
        None
    }
}

/// Which of the three faders are live for the PLAYING item. `vokály` is live when
/// stems are ready OR a not-yet-separated dub is playing (its `vokály` fader IS
/// the whole original); `podklad` is live only with stems (else locked, note
/// `po separácii`); `dabing` is shown + live only when the item has a ready dub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaderAvailability {
    /// The `vokály` fader is interactive.
    pub vokaly: bool,
    /// The `podklad` fader is interactive (else locked "po separácii").
    pub podklad: bool,
    /// The `dabing` fader is shown AND interactive.
    pub dabing: bool,
}

/// Which faders are live from the playing item's stems + dub readiness.
pub fn fader_availability(stems_ready: bool, dub_ready: bool) -> FaderAvailability {
    FaderAvailability {
        vokaly: stems_ready || dub_ready,
        podklad: stems_ready,
        dabing: dub_ready,
    }
}

/// The percent a mixer fader should DISPLAY (and bind to `prop:value`): while the
/// operator is dragging the fader the dragged value is authoritative, so a live
/// gain update from the store (an adapter `Effect`, a re-load) can't overwrite the
/// fader out from under the finger; otherwise the live gain percent drives it.
/// Pure so its boundary is unit-tested (sp-ui has no unit-test job).
pub fn fader_display_pct(dragging: bool, dragged_pct: i32, live_pct: i32) -> i32 {
    if dragging { dragged_pct } else { live_pct }
}

/// Which remembered memory a fader write targets and a `GET`/`PATCH /api/v1/mix`
/// call names (#184 round G1/G2). Songs and dub videos want opposite `vokály`
/// positions almost always, so the console keeps TWO memories — and round G2 drops
/// the GLOBAL "active kind": each reader family is fed from its OWN memory, and the
/// strip edits the memory of ITS item's kind (the API PATCH carries the kind).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixKind {
    /// A plain song (stems or not) — the song memory.
    Song,
    /// A dub video with a ready dub — the dub memory.
    Dub,
}

/// The ONE mixer console with TWO remembered fader triples (#184 round G1/G2). Each
/// memory feeds its OWN reader family — there is NO global "active kind" (round G2):
/// a dub video mixed to `Len dabing` never leaves the next song instrumental-only,
/// and a song at `Plný mix` never doubles a dub video's voices, regardless of what
/// starts on any other output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixConsole {
    /// The song memory. Its `dabing` component is UNUSED (a song has no dub
    /// stream); kept as a `MixFaders` so both memories share one type.
    pub song: MixFaders,
    /// The dub memory (all three faders live).
    pub dub: MixFaders,
}

impl Default for MixConsole {
    /// Song `(1, 1, ·)` = `Plný mix` (bit-exact original), dub `(0, 1, 1)` =
    /// `Len dabing` (dub only by default — the operator raises `vokály` to bring
    /// the original back).
    fn default() -> Self {
        Self {
            song: MixFaders::new(1.0, 1.0, 1.0),
            dub: MixFaders::new(0.0, 1.0, 1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MixFaders construction (clamp + NaN → default) ───────────────────────

    #[test]
    fn mix_faders_default_is_all_full() {
        assert_eq!(
            MixFaders::default(),
            MixFaders {
                vokaly: 1.0,
                podklad: 1.0,
                dabing: 1.0
            }
        );
    }

    #[test]
    fn mix_faders_new_clamps_each_component() {
        assert_eq!(
            MixFaders::new(1.5, -0.5, 0.4),
            MixFaders {
                vokaly: 1.0,
                podklad: 0.0,
                dabing: 0.4
            }
        );
    }

    #[test]
    fn mix_faders_new_any_non_finite_defaults_to_one_one_one() {
        let def = MixFaders::default();
        assert_eq!(MixFaders::new(f32::NAN, 0.2, 0.3), def);
        assert_eq!(MixFaders::new(0.2, f32::INFINITY, 0.3), def);
        assert_eq!(MixFaders::new(0.2, 0.3, f32::NEG_INFINITY), def);
    }

    // ── stream_gains_song ────────────────────────────────────────────────────

    #[test]
    fn stream_gains_song_both_full_is_bit_exact_original() {
        // Both faders full → the untouched original (no separation artefacts).
        assert_eq!(
            stream_gains_song(MixFaders::new(1.0, 1.0, 0.7)),
            [1.0, 0.0, 0.0]
        );
    }

    #[test]
    fn stream_gains_song_mixes_stems_off_the_bit_exact_corner() {
        assert_eq!(
            stream_gains_song(MixFaders::new(0.3, 1.0, 1.0)),
            [0.0, 0.3, 1.0]
        );
        assert_eq!(
            stream_gains_song(MixFaders::new(0.0, 0.6, 1.0)),
            [0.0, 0.0, 0.6]
        );
        // vokály full but podklad reduced → still the mixed branch (not bit-exact).
        assert_eq!(
            stream_gains_song(MixFaders::new(1.0, 0.5, 1.0)),
            [0.0, 1.0, 0.5]
        );
    }

    // ── stream_gains_dub ─────────────────────────────────────────────────────

    #[test]
    fn stream_gains_dub_both_full_is_bit_exact_original_plus_dub() {
        assert_eq!(
            stream_gains_dub(MixFaders::new(1.0, 1.0, 1.0)),
            [1.0, 0.0, 0.0, 1.0]
        );
        // The dub fader is carried even at the bit-exact corner.
        assert_eq!(
            stream_gains_dub(MixFaders::new(1.0, 1.0, 0.5)),
            [1.0, 0.0, 0.0, 0.5]
        );
    }

    #[test]
    fn stream_gains_dub_mixes_stems_plus_dub_off_the_corner() {
        assert_eq!(
            stream_gains_dub(MixFaders::new(0.0, 1.0, 1.0)),
            [0.0, 0.0, 1.0, 1.0]
        );
        assert_eq!(
            stream_gains_dub(MixFaders::new(0.3, 1.0, 1.0)),
            [0.0, 0.3, 1.0, 1.0]
        );
    }

    // ── stream_gains_dub_no_stems (no floor) ─────────────────────────────────

    #[test]
    fn stream_gains_dub_no_stems_is_vokaly_and_dabing_with_no_floor() {
        // Original fully silent at vokály 0 — NO −18 dB floor (round G removes it).
        assert_eq!(
            stream_gains_dub_no_stems(MixFaders::new(0.0, 1.0, 1.0)),
            [0.0, 1.0]
        );
        assert_eq!(
            stream_gains_dub_no_stems(MixFaders::new(1.0, 1.0, 0.0)),
            [1.0, 0.0]
        );
        assert_eq!(
            stream_gains_dub_no_stems(MixFaders::new(0.5, 1.0, 0.5)),
            [0.5, 0.5]
        );
    }

    // ── presets (which buttons show) ─────────────────────────────────────────

    #[test]
    fn presets_are_song_only_without_a_dub() {
        let ids: Vec<&str> = presets(false).iter().map(|p| p.id).collect();
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
    fn presets_add_the_dub_snapshots_with_a_dub() {
        let ids: Vec<&str> = presets(true).iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            [
                "full_mix",
                "karaoke_low",
                "vocals_only",
                "instrumental_only",
                "dub_only",
                "half",
                "original"
            ]
        );
    }

    // ── apply_preset (song presets keep dabing, dub presets pin it) ───────────

    #[test]
    fn apply_song_preset_keeps_the_current_dabing() {
        let cur = MixFaders::new(0.9, 0.1, 0.42);
        // instrumental_only = (0, 1, ·) — dabing untouched.
        let p = SONG_PRESETS[3];
        assert_eq!(apply_preset(&p, cur), MixFaders::new(0.0, 1.0, 0.42));
        // karaoke_low snapshot is (0.3, 1, ·).
        assert_eq!(
            apply_preset(&SONG_PRESETS[1], cur),
            MixFaders::new(0.3, 1.0, 0.42)
        );
    }

    #[test]
    fn apply_dub_preset_pins_all_three() {
        let cur = MixFaders::new(0.9, 0.1, 0.42);
        // dub_only (0,1,1), half (0.5,1,0.5), original (1,1,0).
        assert_eq!(
            apply_preset(&DUB_PRESETS[0], cur),
            MixFaders::new(0.0, 1.0, 1.0)
        );
        assert_eq!(
            apply_preset(&DUB_PRESETS[1], cur),
            MixFaders::new(0.5, 1.0, 0.5)
        );
        assert_eq!(
            apply_preset(&DUB_PRESETS[2], cur),
            MixFaders::new(1.0, 1.0, 0.0)
        );
    }

    // ── preset_for_faders (highlight) ────────────────────────────────────────

    #[test]
    fn preset_for_faders_song_presets_ignore_dabing() {
        // Every song snapshot round-trips (dabing arbitrary).
        assert_eq!(
            preset_for_faders(MixFaders::new(1.0, 1.0, 0.2), false),
            Some("full_mix")
        );
        assert_eq!(
            preset_for_faders(MixFaders::new(0.3, 1.0, 1.0), false),
            Some("karaoke_low")
        );
        assert_eq!(
            preset_for_faders(MixFaders::new(1.0, 0.0, 0.7), false),
            Some("vocals_only")
        );
        assert_eq!(
            preset_for_faders(MixFaders::new(0.0, 1.0, 0.5), false),
            Some("instrumental_only")
        );
    }

    #[test]
    fn preset_for_faders_karaoke_low_is_the_vokaly_below_one_band() {
        // vokály < 1 && podklad == 1 → karaoke_low (the design's rule).
        assert_eq!(
            preset_for_faders(MixFaders::new(0.3, 1.0, 1.0), true),
            Some("karaoke_low")
        );
        // A custom mix (podklad off full) → None.
        assert_eq!(preset_for_faders(MixFaders::new(0.3, 0.7, 1.0), true), None);
    }

    #[test]
    fn preset_for_faders_dub_presets_win_when_a_dub_is_present() {
        // A dub present: the pinned-dabing snapshot is the more specific match.
        assert_eq!(
            preset_for_faders(MixFaders::new(0.0, 1.0, 1.0), true),
            Some("dub_only")
        );
        assert_eq!(
            preset_for_faders(MixFaders::new(0.5, 1.0, 0.5), true),
            Some("half")
        );
        assert_eq!(
            preset_for_faders(MixFaders::new(1.0, 1.0, 0.0), true),
            Some("original")
        );
        // Same faders but WITHOUT a dub → the song reading (dabing ignored).
        assert_eq!(
            preset_for_faders(MixFaders::new(0.0, 1.0, 1.0), false),
            Some("instrumental_only")
        );
    }

    #[test]
    fn preset_for_faders_none_for_a_custom_mix() {
        assert_eq!(preset_for_faders(MixFaders::new(0.5, 0.5, 1.0), true), None);
        assert_eq!(
            preset_for_faders(MixFaders::new(0.0, 0.0, 1.0), false),
            None
        );
    }

    #[test]
    fn preset_for_faders_tolerates_the_integer_percent_rounding() {
        // podklad 0.995 (a 99->100 % round) still matches half's podklad==1 within
        // 0.01 when vokály + dabing are on the half snapshot.
        assert_eq!(
            preset_for_faders(MixFaders::new(0.5, 0.995, 0.5), true),
            Some("half")
        );
        // dabing off by 0.02 → no dub match, falls to the song reading (karaoke).
        assert_eq!(
            preset_for_faders(MixFaders::new(0.5, 1.0, 0.52), true),
            Some("karaoke_low")
        );
    }

    // ── fader_availability ───────────────────────────────────────────────────

    #[test]
    fn fader_availability_plain_song_no_stems_locks_both() {
        assert_eq!(
            fader_availability(false, false),
            FaderAvailability {
                vokaly: false,
                podklad: false,
                dabing: false
            }
        );
    }

    #[test]
    fn fader_availability_stems_song_opens_vokaly_and_podklad() {
        assert_eq!(
            fader_availability(true, false),
            FaderAvailability {
                vokaly: true,
                podklad: true,
                dabing: false
            }
        );
    }

    #[test]
    fn fader_availability_no_stems_dub_opens_vokaly_and_dabing_not_podklad() {
        assert_eq!(
            fader_availability(false, true),
            FaderAvailability {
                vokaly: true,
                podklad: false,
                dabing: true
            }
        );
    }

    #[test]
    fn fader_availability_stems_dub_opens_all_three() {
        assert_eq!(
            fader_availability(true, true),
            FaderAvailability {
                vokaly: true,
                podklad: true,
                dabing: true
            }
        );
    }

    // ── fader_display_pct ────────────────────────────────────────────────────

    #[test]
    fn fader_display_dragging_returns_the_dragged_pct() {
        assert_eq!(fader_display_pct(true, 40, 100), 40);
    }

    #[test]
    fn fader_display_not_dragging_returns_the_live_pct() {
        assert_eq!(fader_display_pct(false, 40, 100), 100);
    }

    // ── MixKind + MixConsole (two kind-scoped memories, no active kind) ───────

    #[test]
    fn mix_console_default_is_song_full_and_dub_len_dabing() {
        // Round G2: two memories, no active kind.
        let c = MixConsole::default();
        assert_eq!(c.song, MixFaders::new(1.0, 1.0, 1.0));
        assert_eq!(c.dub, MixFaders::new(0.0, 1.0, 1.0));
    }

    #[test]
    fn mix_kind_is_song_and_dub_and_compares_by_value() {
        // The two API/control selectors are distinct and Eq-comparable.
        assert_eq!(MixKind::Song, MixKind::Song);
        assert_eq!(MixKind::Dub, MixKind::Dub);
        assert_ne!(MixKind::Song, MixKind::Dub);
    }

    #[test]
    fn mix_console_holds_both_memories_independently() {
        let c = MixConsole {
            song: MixFaders::new(0.7, 0.6, 1.0),
            dub: MixFaders::new(0.2, 0.9, 0.4),
        };
        assert_eq!(c.song, MixFaders::new(0.7, 0.6, 1.0));
        assert_eq!(c.dub, MixFaders::new(0.2, 0.9, 0.4));
    }
}
