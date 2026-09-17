//! Process-global live karaoke control (#14, #186).
//!
//! The karaoke mode + vocal gain are a single app-wide live setting (one wall,
//! one operator flipping it during a service), so — unlike the per-output
//! `burn_on` registry — they live in ONE process-global [`KaraokeControl`].
//!
//! Since #186 a karaoke MODE is a live gain PRESET, not a choice of which files
//! to open. The control holds three LIVE target-gain atomics
//! `[original, vocals, instrumental]`; `set_mode` / `set_vocal_gain` publish the
//! preset triple to them, and every playing [`sp_decoder::StemMixReader`] reads
//! and ramps toward them — so a preset change is heard immediately, with NO
//! pipeline reopen (the seconds-of-silence dropout #186 fixes).

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use sp_core::playback::KaraokeMode;

/// Live karaoke mode + vocal gain, shared between the API/engine (writers) and
/// every playback pipeline (reader).
pub struct KaraokeControl {
    /// `KaraokeMode::as_u8` — the active preset (read when a song opens, and to
    /// re-derive the gain triple on a fader move).
    mode: AtomicU8,
    /// f32 bits, `0.0..=1.0` — the operator's stored vocal-fader POSITION (the
    /// `vg` input to the KaraokeLow preset), NOT a gain handed to the mixer.
    vocal_gain: Arc<AtomicU32>,
    /// The LIVE per-stream target gains `[original, vocals, instrumental]` every
    /// [`sp_decoder::StemMixReader`] reads and ramps toward. `set_mode` /
    /// `set_vocal_gain` write the preset triple here so a preset change reaches
    /// the playing mixer WITHOUT reopening the pipeline (#186).
    gains: [Arc<AtomicU32>; 3],
}

/// Default vocal gain for KaraokeLow when no setting is stored (30 %).
pub const DEFAULT_VOCAL_GAIN: f32 = 0.3;

/// The per-stream linear gains `(original, vocals, instrumental)` for a karaoke
/// mode PRESET (#186). `vocal_gain` (clamped) only scales the KaraokeLow vocals:
///
/// | mode              | original | vocals | instrumental |
/// |-------------------|----------|--------|--------------|
/// | FullMix           | 1        | 0      | 0            |
/// | KaraokeLow        | 0        | vg     | 1            |
/// | VocalsOnly        | 0        | 1      | 0            |
/// | InstrumentalOnly  | 0        | 0      | 1            |
///
/// FullMix plays the ORIGINAL alone (bit-exact, no separation artefacts); every
/// non-FullMix preset drops the original and mixes the two stems.
pub fn preset_gains(mode: KaraokeMode, vocal_gain: f32) -> (f32, f32, f32) {
    let vg = clamp_gain(vocal_gain);
    match mode {
        KaraokeMode::FullMix => (1.0, 0.0, 0.0),
        KaraokeMode::KaraokeLow => (0.0, vg, 1.0),
        KaraokeMode::VocalsOnly => (0.0, 1.0, 0.0),
        KaraokeMode::InstrumentalOnly => (0.0, 0.0, 1.0),
    }
}

impl KaraokeControl {
    fn new(mode: KaraokeMode, vocal_gain: f32) -> Self {
        let vg = clamp_gain(vocal_gain);
        let (o, v, i) = preset_gains(mode, vg);
        Self {
            mode: AtomicU8::new(mode.as_u8()),
            vocal_gain: Arc::new(AtomicU32::new(vg.to_bits())),
            gains: [
                Arc::new(AtomicU32::new(o.to_bits())),
                Arc::new(AtomicU32::new(v.to_bits())),
                Arc::new(AtomicU32::new(i.to_bits())),
            ],
        }
    }

    /// Current karaoke mode.
    pub fn mode(&self) -> KaraokeMode {
        KaraokeMode::from_u8(self.mode.load(Ordering::Relaxed))
    }

    /// Set the karaoke mode and publish the new preset triple to the live gain
    /// atomics — a preset change the playing mixer picks up with no reopen (#186).
    pub fn set_mode(&self, mode: KaraokeMode) {
        self.mode.store(mode.as_u8(), Ordering::Relaxed);
        self.write_preset(mode, self.vocal_gain());
    }

    /// Current vocal gain (`0.0..=1.0`) — the stored fader position.
    pub fn vocal_gain(&self) -> f32 {
        f32::from_bits(self.vocal_gain.load(Ordering::Relaxed))
    }

    /// Set the vocal fader (clamped to `0.0..=1.0`) and re-derive the live gain
    /// triple. Takes effect live — the playing `StemMixReader` ramps toward the
    /// new `vocals` gain. (The fader only changes the KaraokeLow preset; for the
    /// other presets re-publishing the triple is a harmless no-op.)
    pub fn set_vocal_gain(&self, gain: f32) {
        let vg = clamp_gain(gain);
        self.vocal_gain.store(vg.to_bits(), Ordering::Relaxed);
        self.write_preset(self.mode(), vg);
    }

    /// Recompute the preset gain triple and publish it to the live `gains`
    /// atomics the mixer reads — the seam that makes a preset / fader change
    /// audible with no pipeline reopen (#186).
    fn write_preset(&self, mode: KaraokeMode, vocal_gain: f32) {
        let (o, v, i) = preset_gains(mode, vocal_gain);
        self.gains[0].store(o.to_bits(), Ordering::Relaxed);
        self.gains[1].store(v.to_bits(), Ordering::Relaxed);
        self.gains[2].store(i.to_bits(), Ordering::Relaxed);
    }

    /// Clone the three LIVE gain atomics `[original, vocals, instrumental]` to
    /// hand a `StemMixReader`, so a preset / fader change is heard immediately
    /// mid-song without reopening the pipeline.
    pub fn gain_handles(&self) -> [Arc<AtomicU32>; 3] {
        [
            Arc::clone(&self.gains[0]),
            Arc::clone(&self.gains[1]),
            Arc::clone(&self.gains[2]),
        ]
    }

    /// Construct a standalone control for tests (not the process global).
    #[cfg(test)]
    pub(crate) fn new_for_test(mode: KaraokeMode, vocal_gain: f32) -> Self {
        Self::new(mode, vocal_gain)
    }
}

fn clamp_gain(g: f32) -> f32 {
    if g.is_finite() {
        g.clamp(0.0, 1.0)
    } else {
        DEFAULT_VOCAL_GAIN
    }
}

static GLOBAL: OnceLock<Arc<KaraokeControl>> = OnceLock::new();

/// Initialise the process-global control from the stored settings. Idempotent —
/// the first call wins; later calls just set the values on the existing control.
pub fn init(mode: KaraokeMode, vocal_gain: f32) -> Arc<KaraokeControl> {
    let ctrl = GLOBAL.get_or_init(|| Arc::new(KaraokeControl::new(mode, vocal_gain)));
    // If the global already existed (e.g. a test set it first), reconcile.
    ctrl.set_mode(mode);
    ctrl.set_vocal_gain(vocal_gain);
    Arc::clone(ctrl)
}

/// The process-global karaoke control, lazily defaulting to `FullMix` + the
/// default vocal gain if `init` was never called (unit tests, degraded boot).
pub fn global() -> Arc<KaraokeControl> {
    Arc::clone(GLOBAL.get_or_init(|| {
        Arc::new(KaraokeControl::new(
            KaraokeMode::FullMix,
            DEFAULT_VOCAL_GAIN,
        ))
    }))
}

/// Seed the process-global control from the `karaoke_mode` + `karaoke_vocal_gain`
/// DB settings, so a restart restores the operator's last choice and pipelines
/// pick it up at song open.
pub async fn init_from_settings(pool: &sqlx::SqlitePool) -> Arc<KaraokeControl> {
    let mode = crate::db::models::get_setting(pool, "karaoke_mode")
        .await
        .ok()
        .flatten()
        .map(|s| KaraokeMode::from_str_lossy(&s))
        .unwrap_or(KaraokeMode::FullMix);
    let gain = crate::db::models::get_setting(pool, "karaoke_vocal_gain")
        .await
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(DEFAULT_VOCAL_GAIN);
    init(mode, gain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_control_holds_mode_and_gain() {
        let c = KaraokeControl::new(KaraokeMode::KaraokeLow, 0.4);
        assert_eq!(c.mode(), KaraokeMode::KaraokeLow);
        assert!((c.vocal_gain() - 0.4).abs() < 1e-6);
    }

    #[test]
    fn set_mode_and_gain_update() {
        let c = KaraokeControl::new(KaraokeMode::FullMix, 0.3);
        c.set_mode(KaraokeMode::InstrumentalOnly);
        c.set_vocal_gain(0.75);
        assert_eq!(c.mode(), KaraokeMode::InstrumentalOnly);
        assert!((c.vocal_gain() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn vocal_gain_is_clamped_and_nan_guarded() {
        let c = KaraokeControl::new(KaraokeMode::FullMix, 0.3);
        c.set_vocal_gain(2.0);
        assert!((c.vocal_gain() - 1.0).abs() < 1e-6);
        c.set_vocal_gain(-1.0);
        assert!((c.vocal_gain() - 0.0).abs() < 1e-6);
        c.set_vocal_gain(f32::NAN);
        assert!((c.vocal_gain() - DEFAULT_VOCAL_GAIN).abs() < 1e-6);
    }

    fn read(a: &Arc<AtomicU32>) -> f32 {
        f32::from_bits(a.load(Ordering::Relaxed))
    }

    #[test]
    fn preset_gains_table() {
        assert_eq!(preset_gains(KaraokeMode::FullMix, 0.3), (1.0, 0.0, 0.0));
        assert_eq!(preset_gains(KaraokeMode::KaraokeLow, 0.3), (0.0, 0.3, 1.0));
        assert_eq!(preset_gains(KaraokeMode::VocalsOnly, 0.3), (0.0, 1.0, 0.0));
        assert_eq!(
            preset_gains(KaraokeMode::InstrumentalOnly, 0.3),
            (0.0, 0.0, 1.0)
        );
        // vg only scales the KaraokeLow vocals stream.
        assert_eq!(preset_gains(KaraokeMode::KaraokeLow, 0.8), (0.0, 0.8, 1.0));
    }

    #[test]
    fn preset_gains_clamps_and_guards_nan() {
        assert_eq!(preset_gains(KaraokeMode::KaraokeLow, 1.5), (0.0, 1.0, 1.0));
        assert_eq!(preset_gains(KaraokeMode::KaraokeLow, -0.5), (0.0, 0.0, 1.0));
        assert_eq!(
            preset_gains(KaraokeMode::KaraokeLow, f32::NAN),
            (0.0, DEFAULT_VOCAL_GAIN, 1.0)
        );
    }

    #[test]
    fn gain_handles_publish_preset_and_are_live() {
        let c = KaraokeControl::new(KaraokeMode::FullMix, 0.3);
        let [o, v, i] = c.gain_handles();
        // FullMix preset: original alone.
        assert_eq!((read(&o), read(&v), read(&i)), (1.0, 0.0, 0.0));
        // A mode change publishes the new triple to the SAME atomics (live).
        c.set_mode(KaraokeMode::InstrumentalOnly);
        assert_eq!((read(&o), read(&v), read(&i)), (0.0, 0.0, 1.0));
        // KaraokeLow + a fader move: vocals scale live, original stays 0.
        c.set_mode(KaraokeMode::KaraokeLow);
        c.set_vocal_gain(0.6);
        assert_eq!((read(&o), read(&v), read(&i)), (0.0, 0.6, 1.0));
    }
}
